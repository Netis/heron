//! Optional packet dump to pcap files.
//!
//! Writes every non-heartbeat [`RawPacket`] a source produces to a
//! Wireshark-openable classic pcap file under `<dir>/<sanitized_source_id>/`,
//! rotating on wall-clock minute boundaries (by packet timestamp). Sparse —
//! minutes with no packets produce no file. Optional snappy framed
//! compression appends `.snappy` to the filename. Disabled by default;
//! enabled per pipeline via `[pipeline.pcap_dump]`. Dump failures are
//! logged and the offending source is muted — capture never fails on
//! behalf of the dumper.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

use snap::write::FrameEncoder;

use h_common::config::{PcapCompression, PcapDumpConfig};
use h_common::internal_metrics::{Metric, MetricsWorker};
use h_common::throttle::ThrottledWarn;

use crate::packet::RawPacket;

const WARN_THROTTLE: Duration = Duration::from_secs(5);
const BUF_CAPACITY: usize = 64 * 1024;
const PCAP_MAGIC: u32 = 0xa1b2_c3d4;
const PCAP_VERSION_MAJOR: u16 = 2;
const PCAP_VERSION_MINOR: u16 = 4;
// Aligns with `default_snaplen()` in h-common (262_144). Declaring an
// upper bound ≥ actual caplen is required by the pcap spec; declaring it
// smaller than a real record's caplen — as the original 65535 did when
// paired with the project's default 262_144 capture snaplen — violates
// caplen ≤ snaplen and trips strict readers. Declaring it larger is
// always safe (header snaplen is just an advertised upper bound).
const PCAP_SNAPLEN: u32 = 262_144;
const MICROS_PER_MINUTE: i64 = 60 * 1_000_000;

/// Writer config resolved from [`PcapDumpConfig`].
#[derive(Debug, Clone)]
pub struct PacketDumperConfig {
    pub dir: PathBuf,
    pub compression: PcapCompression,
}

impl PacketDumperConfig {
    pub fn from_config(cfg: &PcapDumpConfig) -> Self {
        Self {
            dir: PathBuf::from(&cfg.dir),
            compression: cfg.compression,
        }
    }
}

/// Lazily opens one [`MinuteFile`] per source per wall-clock minute.
/// Each source's `link_type` is pinned at first packet and persists across
/// minute rotations; mismatches drop the offending packet (throttled warn).
pub struct PacketDumper {
    cfg: PacketDumperConfig,
    writers: HashMap<String, SourceWriter>,
    disabled: HashSet<String>,
    err_throttle: ThrottledWarn,
    metrics: MetricsWorker,
}

struct SourceWriter {
    src_dir: PathBuf,
    link_type: u32,
    current: Option<MinuteFile>,
}

struct MinuteFile {
    minute_key: i64,
    sink: Sink,
}

/// Output sink for one minute file. Owned (not boxed) so we can reach the
/// inner `BufWriter` to cascade flushes — `FrameEncoder::flush` only drains
/// its own source buffer into the BufWriter, it does NOT then flush the
/// BufWriter to the file. Heartbeat-driven `flush_all` must walk both layers.
enum Sink {
    Plain(BufWriter<File>),
    Snappy(FrameEncoder<BufWriter<File>>),
}

impl Sink {
    /// Flush every layer down to the kernel. For snappy: drains the
    /// FrameEncoder's source buffer (emitting a possibly-partial frame),
    /// then flushes the underlying BufWriter to the file.
    fn flush_all_layers(&mut self) -> io::Result<()> {
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Snappy(w) => {
                w.flush()?;
                w.get_mut().flush()
            }
        }
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Plain(w) => w.write(buf),
            Sink::Snappy(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Note: this only flushes the topmost layer. Use `flush_all_layers`
        // when you need durability through the BufWriter.
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Snappy(w) => w.flush(),
        }
    }
}

impl PacketDumper {
    /// Create the output directory if missing.
    pub fn new(cfg: PacketDumperConfig, metrics: MetricsWorker) -> io::Result<Self> {
        std::fs::create_dir_all(&cfg.dir)?;
        Ok(Self {
            cfg,
            writers: HashMap::new(),
            disabled: HashSet::new(),
            err_throttle: ThrottledWarn::new(WARN_THROTTLE),
            metrics,
        })
    }

    /// Write one packet. Heartbeats are silently skipped. All I/O errors are
    /// swallowed — the offending source is muted so capture can continue.
    ///
    /// **Borrow-check note:** the body is split into phases that each scope
    /// their `&mut SourceWriter` borrow. Calling `self.note_error()` is
    /// illegal while `source` is held, so each error path either extracts
    /// values first (link_type check uses NLL release) or scopes the source
    /// borrow inside a block.
    pub fn write(&mut self, pkt: &RawPacket) {
        if pkt.is_heartbeat() {
            return;
        }
        if self.disabled.contains(&pkt.source_id) {
            return;
        }

        // Phase 1: lazy source creation.
        if !self.writers.contains_key(&pkt.source_id) {
            match self.open_source_dir(&pkt.source_id, pkt.link_type) {
                Some(sw) => {
                    self.writers.insert(pkt.source_id.clone(), sw);
                }
                None => return, // already logged + disabled
            }
        }

        let pkt_minute = pkt.timestamp_us.div_euclid(MICROS_PER_MINUTE);

        // Phase 2: link_type check. NLL releases the borrow before
        // `self.note_error` because all uses of `source` after this point
        // are Copy-value reads.
        {
            let source = self.writers.get(&pkt.source_id).expect("just inserted");
            if source.link_type != pkt.link_type {
                let pinned = source.link_type;
                let got = pkt.link_type;
                let sid = pkt.source_id.clone();
                self.note_error(&format!(
                    "pcap-dump: link_type mismatch on source '{sid}' (pinned={pinned}, got={got}); dropping packet"
                ));
                return;
            }
        }

        // Phase 3: decide whether this packet starts a new minute file.
        let need_new_file = {
            let source = self.writers.get(&pkt.source_id).expect("just inserted");
            match &source.current {
                None => true,
                Some(cur) => pkt_minute > cur.minute_key,
            }
        };

        // Phase 4: rotate. Open the new file *outside* the `source` borrow
        // scope so that on Err we can call `self.note_error` cleanly.
        if need_new_file {
            let (src_dir, link_type, compression) = {
                let source = self.writers.get(&pkt.source_id).expect("just inserted");
                (
                    source.src_dir.clone(),
                    source.link_type,
                    self.cfg.compression,
                )
            };
            // Drop the old file so its BufWriter / FrameEncoder finalize.
            self.writers.get_mut(&pkt.source_id).unwrap().current = None;
            match Self::open_minute_file(&src_dir, pkt_minute, link_type, compression) {
                Ok(mf) => {
                    self.writers.get_mut(&pkt.source_id).unwrap().current = Some(mf);
                }
                Err(e) => {
                    let sid = pkt.source_id.clone();
                    self.note_error(&format!(
                        "pcap-dump: failed to open minute file for source '{sid}': {e}"
                    ));
                    self.disabled.insert(sid);
                    return;
                }
            }
        }

        // Phase 5: write the record. Compute `is_late` and write inside a
        // scoped block; emit the late-minute counter and any write error
        // only after the source borrow ends.
        let (write_result, is_late) = {
            let source = self.writers.get_mut(&pkt.source_id).expect("just inserted");
            let cur = source
                .current
                .as_mut()
                .expect("just opened or pre-existing");
            let is_late = pkt_minute < cur.minute_key;
            let write_result = write_pcap_record(&mut cur.sink, pkt);
            (write_result, is_late)
        };

        if is_late {
            self.metrics
                .counter(Metric::CaptureDumpLateMinutePackets)
                .inc();
        }

        if let Err(e) = write_result {
            let sid = pkt.source_id.clone();
            self.note_error(&format!("pcap-dump: write failed for source '{sid}': {e}"));
            // Keep the source enabled — a single write failure shouldn't
            // kill the whole source. Next packet retries.
        }
    }

    /// Flush every open minute file's buffers to the kernel. Safe to call
    /// from the shutdown path or on each heartbeat to bound data loss on
    /// hard termination.
    pub fn flush_all(&mut self) {
        let mut errs: Vec<(String, String)> = Vec::new();
        for (sid, sw) in self.writers.iter_mut() {
            if let Some(cur) = sw.current.as_mut() {
                if let Err(e) = cur.sink.flush_all_layers() {
                    errs.push((sid.clone(), e.to_string()));
                }
            }
        }
        for (sid, e) in errs {
            self.note_error(&format!("pcap-dump: flush failed for source '{sid}': {e}"));
        }
    }

    fn open_source_dir(&mut self, source_id: &str, link_type: u32) -> Option<SourceWriter> {
        let safe = match sanitize(source_id) {
            Some(s) => s,
            None => {
                let sid = source_id.to_string();
                self.note_error(&format!("pcap-dump: refusing source '{sid}': invalid id"));
                self.disabled.insert(sid);
                return None;
            }
        };
        let src_dir = self.cfg.dir.join(&safe);
        // Defence-in-depth: refuse any path that escapes the dump dir.
        if !src_dir.starts_with(&self.cfg.dir) {
            let p = src_dir.display().to_string();
            self.note_error(&format!("pcap-dump: source dir '{p}' escapes dump dir"));
            self.disabled.insert(source_id.to_string());
            return None;
        }
        if let Err(e) = std::fs::create_dir_all(&src_dir) {
            let p = src_dir.display().to_string();
            self.note_error(&format!("pcap-dump: mkdir '{p}' failed: {e}"));
            self.disabled.insert(source_id.to_string());
            return None;
        }
        tracing::info!(
            "pcap-dump: source '{source_id}' → {} (link_type={link_type}, compression={:?})",
            src_dir.display(),
            self.cfg.compression,
        );
        Some(SourceWriter {
            src_dir,
            link_type,
            current: None,
        })
    }

    /// Open `<src_dir>/<minute>.pcap[.snappy]`. If the file already exists
    /// (process restarted within the same minute for the same source),
    /// append to it instead of truncating — preserves data already flushed
    /// to disk under the heartbeat-flush durability contract.
    ///
    /// On creation, write the 24-byte global header. On append, skip it (the
    /// existing header is reused). For snappy, appending a new framed segment
    /// is well-defined: the frame stream is self-delimiting and decoders read
    /// concatenated frames as one continuous stream.
    ///
    /// Caveat: if the source's `link_type` somehow differs across the
    /// restart, the file would mix link types. In practice the same
    /// `source_id` always maps to the same NIC / probe, so this is
    /// theoretical — we trust same-source = same-link_type and skip
    /// validating the existing header.
    ///
    /// On global-header failure during creation, best-effort removes the
    /// half-born file so libpcap (which rejects 0-byte files) and operators
    /// don't see it.
    fn open_minute_file(
        src_dir: &std::path::Path,
        minute_key: i64,
        link_type: u32,
        compression: PcapCompression,
    ) -> io::Result<MinuteFile> {
        let filename = format!(
            "{}.pcap{}",
            minute_label(minute_key),
            match compression {
                PcapCompression::None => "",
                PcapCompression::Snappy => ".snappy",
            }
        );
        let path = src_dir.join(&filename);
        // Retry once on ENOENT after recreating the source dir. Covers the
        // case where the dir was removed out from under us (operator rm,
        // or — historically — the retention sweeper). Cheap when the dir
        // exists; recovery cost paid only when the rare race fires.
        let file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir_all(src_dir)?;
                OpenOptions::new().create(true).append(true).open(&path)?
            }
            Err(e) => return Err(e),
        };
        let needs_global_header = file.metadata()?.len() == 0;
        let buf = BufWriter::with_capacity(BUF_CAPACITY, file);
        let mut sink = match compression {
            PcapCompression::None => Sink::Plain(buf),
            PcapCompression::Snappy => Sink::Snappy(FrameEncoder::new(buf)),
        };
        if needs_global_header {
            if let Err(e) = write_pcap_global_header(&mut sink, link_type) {
                // Best-effort cleanup; ignore remove errors (e.g. double-failure
                // on disk-full). Return the original error.
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        }
        Ok(MinuteFile { minute_key, sink })
    }

    fn note_error(&mut self, msg: &str) {
        self.metrics.counter(Metric::CaptureDumpErrors).inc();
        if let Some(suppressed) = self.err_throttle.tick() {
            if suppressed > 0 {
                tracing::warn!(suppressed, "{msg} (latest of many)");
            } else {
                tracing::warn!("{msg}");
            }
        }
    }
}

/// Resolve the per-pipeline dump root: `<base>/<sanitized_pipeline_name>`.
/// Each pipeline gets its own subtree so the dumper and retention sweeper
/// stay fully isolated even when multiple pipelines configure the same
/// `pcap_dump.dir`. Returns `None` for pipeline names that would produce
/// an unsafe path (empty, `.`, `..`). The retention task and dumper both
/// take this composed path as their root.
pub fn pcap_dump_dir_for(base: &std::path::Path, pipeline_name: &str) -> Option<PathBuf> {
    sanitize(pipeline_name).map(|s| base.join(s))
}

/// Sanitize a single path component. Delegates to
/// [`h_common::path::sanitize_path_component`] so config validation
/// (`AppConfig::validate`) and the dumper share one rule.
fn sanitize(source_id: &str) -> Option<String> {
    h_common::path::sanitize_path_component(source_id)
}

/// Compact UTC label for a wall-clock minute. `minute_key` is
/// `pkt.timestamp_us / 60_000_000`. Returns e.g. `"20260501T1530"`.
pub(crate) fn minute_label(minute_key: i64) -> String {
    let total_secs = minute_key * 60;
    let days = total_secs.div_euclid(86_400);
    let rem = total_secs.rem_euclid(86_400);
    let (h, m) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32);
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}")
}

/// Inverse of [`minute_label`]. Returns `None` for any string that doesn't
/// match the exact `YYYYMMDDTHHMM` shape produced by `minute_label`, or whose
/// fields don't form a valid UTC instant. Callers (e.g. `pcap_retention`)
/// rely on this strict round-trip property to safely reap files we created.
pub(crate) fn parse_minute_label(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 13 || b[8] != b'T' {
        return None;
    }
    if !b[..8].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !b[9..].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let mo: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    let h: u32 = s[9..11].parse().ok()?;
    let mi: u32 = s[11..13].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 {
        return None;
    }
    let days = ymd_to_days(y, mo, d);
    // Round-trip: rejects invalid day-of-month (e.g. Feb 30, Apr 31).
    if days_to_ymd(days) != (y, mo, d) {
        return None;
    }
    Some(days * 1440 + i64::from(h) * 60 + i64::from(mi))
}

/// Inverse of [`days_to_ymd`]: convert (year, month, day) UTC to
/// days-since-1970-01-01. Days-from-civil algorithm by Howard Hinnant.
fn ymd_to_days(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 {
        i64::from(y) - 1
    } else {
        i64::from(y)
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_shift = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * m_shift + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Convert days-since-1970-01-01 (UTC) to (year, month, day). Civil-from-days
/// algorithm by Howard Hinnant.
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

/// Write the 24-byte classic-pcap global header (microsecond magic).
fn write_pcap_global_header<W: Write>(w: &mut W, link_type: u32) -> io::Result<()> {
    w.write_all(&PCAP_MAGIC.to_le_bytes())?;
    w.write_all(&PCAP_VERSION_MAJOR.to_le_bytes())?;
    w.write_all(&PCAP_VERSION_MINOR.to_le_bytes())?;
    w.write_all(&0i32.to_le_bytes())?; // thiszone
    w.write_all(&0u32.to_le_bytes())?; // sigfigs
    w.write_all(&PCAP_SNAPLEN.to_le_bytes())?;
    w.write_all(&link_type.to_le_bytes())?;
    Ok(())
}

/// Write one 16-byte record header followed by the packet bytes.
fn write_pcap_record<W: Write>(w: &mut W, pkt: &RawPacket) -> io::Result<()> {
    let ts_sec = (pkt.timestamp_us / 1_000_000) as u32;
    let ts_usec = (pkt.timestamp_us % 1_000_000) as u32;
    w.write_all(&ts_sec.to_le_bytes())?;
    w.write_all(&ts_usec.to_le_bytes())?;
    w.write_all(&pkt.caplen.to_le_bytes())?;
    w.write_all(&pkt.wirelen.to_le_bytes())?;
    w.write_all(&pkt.data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pcap::Capture;
    use std::path::Path;
    use h_common::internal_metrics::MetricsSystem;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        sys.register_worker(
            "test",
            &[
                Metric::CaptureDumpErrors,
                Metric::CaptureDumpLateMinutePackets,
            ],
        )
    }

    fn make_pkt(source_id: &str, ts_us: i64, data: &[u8]) -> RawPacket {
        RawPacket {
            timestamp_us: ts_us,
            caplen: data.len() as u32,
            wirelen: data.len() as u32,
            link_type: 1,
            data: Bytes::copy_from_slice(data),
            source_id: source_id.to_string(),
        }
    }

    fn cfg(dir: &Path) -> PacketDumperConfig {
        PacketDumperConfig {
            dir: dir.to_path_buf(),
            compression: PcapCompression::None,
        }
    }

    fn cfg_snappy(dir: &Path) -> PacketDumperConfig {
        PacketDumperConfig {
            dir: dir.to_path_buf(),
            compression: PcapCompression::Snappy,
        }
    }

    fn read_pcap(path: &Path) -> (u32, Vec<(i64, Vec<u8>)>) {
        let mut cap = Capture::from_file(path).expect("open pcap");
        let lt = cap.get_datalink().0 as u32;
        let mut out = Vec::new();
        while let Ok(p) = cap.next_packet() {
            let ts = p.header.ts.tv_sec as i64 * 1_000_000 + p.header.ts.tv_usec as i64;
            out.push((ts, p.data.to_vec()));
        }
        (lt, out)
    }

    /// Decompress a snappy-framed file to a sibling temp `.pcap`, then read with libpcap.
    fn read_snappy_pcap(path: &Path) -> (u32, Vec<(i64, Vec<u8>)>) {
        use std::io::Read as _;
        let f = std::fs::File::open(path).expect("open snappy");
        let mut dec = snap::read::FrameDecoder::new(f);
        let mut buf = Vec::new();
        dec.read_to_end(&mut buf).expect("decompress");
        let dir = path.parent().unwrap();
        let plain = dir.join("__decompressed.pcap");
        std::fs::write(&plain, &buf).unwrap();
        read_pcap(&plain)
    }

    // ---- encoder helpers (unit-level) -----------------------------------

    #[test]
    fn global_header_is_24_bytes() {
        let mut buf = Vec::new();
        write_pcap_global_header(&mut buf, 1).unwrap();
        assert_eq!(buf.len(), 24);
        assert_eq!(&buf[0..4], &0xa1b2_c3d4u32.to_le_bytes());
        assert_eq!(&buf[20..24], &1u32.to_le_bytes());
    }

    #[test]
    fn record_layout_matches_pcap_spec() {
        let pkt = make_pkt("s", 1_500_250, &[0xaa, 0xbb, 0xcc]);
        let mut buf = Vec::new();
        write_pcap_record(&mut buf, &pkt).unwrap();
        assert_eq!(buf.len(), 19); // 16 header + 3 data
        assert_eq!(&buf[0..4], &1u32.to_le_bytes());
        assert_eq!(&buf[4..8], &500_250u32.to_le_bytes());
        assert_eq!(&buf[8..12], &3u32.to_le_bytes());
        assert_eq!(&buf[12..16], &3u32.to_le_bytes());
        assert_eq!(&buf[16..19], &[0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn round_trip_two_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("a", 1_000_000, &[0x01, 0x02, 0x03]));
        d.write(&make_pkt("b", 2_500_000, &[0xaa, 0xbb]));
        d.write(&make_pkt("a", 3_000_250, &[0x04]));
        drop(d);

        // Both packets for "a" land in the minute-0 file.
        let (lt_a, pkts_a) = read_pcap(&dir.path().join("a/19700101T0000.pcap"));
        assert_eq!(lt_a, 1);
        assert_eq!(pkts_a.len(), 2);
        assert_eq!(pkts_a[0], (1_000_000, vec![0x01, 0x02, 0x03]));
        assert_eq!(pkts_a[1], (3_000_250, vec![0x04]));

        let (_, pkts_b) = read_pcap(&dir.path().join("b/19700101T0000.pcap"));
        assert_eq!(pkts_b.len(), 1);
        assert_eq!(pkts_b[0], (2_500_000, vec![0xaa, 0xbb]));
    }

    #[test]
    fn heartbeats_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("s", 1_000_000, &[0x01]));
        d.write(&RawPacket::heartbeat(1_500_000, "s".to_string()));
        d.write(&make_pkt("s", 2_000_000, &[0x02]));
        drop(d);

        let (_, pkts) = read_pcap(&dir.path().join("s/19700101T0000.pcap"));
        assert_eq!(pkts.len(), 2);
    }

    #[test]
    fn link_type_mismatch_is_dropped_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("s", 1_000_000, &[0x01]));
        let mut bad = make_pkt("s", 2_000_000, &[0x02]);
        bad.link_type = 101; // Raw IP vs pinned Ethernet
        d.write(&bad);
        drop(d);

        let (_, pkts) = read_pcap(&dir.path().join("s/19700101T0000.pcap"));
        assert_eq!(pkts.len(), 1, "mismatched link_type packet must be dropped");
    }

    #[test]
    fn source_id_is_sanitized_and_stays_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("evil/../x", 1_000_000, &[0x01]));
        drop(d);

        let expected = dir.path().join("evil_.._x/19700101T0000.pcap");
        assert!(expected.is_file(), "expected {}", expected.display());
        // Nothing escaped into the parent dir.
        let parent = dir.path().parent().unwrap();
        assert!(!parent.join("x.pcap").exists());
    }

    #[test]
    fn empty_source_id_disables_that_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("", 1_000_000, &[0x01]));
        d.write(&make_pkt("", 2_000_000, &[0x02])); // still no-op
        drop(d);

        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn days_to_ymd_known_dates() {
        assert_eq!(super::days_to_ymd(0), (1970, 1, 1));
        assert_eq!(super::days_to_ymd(31), (1970, 2, 1));
        assert_eq!(super::days_to_ymd(365), (1971, 1, 1));
        assert_eq!(super::days_to_ymd(10_957), (2000, 1, 1));
        assert_eq!(super::days_to_ymd(10_956), (1999, 12, 31));
    }

    #[test]
    fn minute_label_format() {
        // 2026-05-05 13:30 UTC → seconds since epoch = 1777987800; minute_key = 29633130
        let key = 1_777_987_800 / 60;
        assert_eq!(minute_label(key), "20260505T1330");
    }

    #[test]
    fn pcap_dump_dir_for_appends_sanitized_pipeline_name() {
        let base = Path::new("/var/lib/dumps");
        assert_eq!(
            pcap_dump_dir_for(base, "local"),
            Some(PathBuf::from("/var/lib/dumps/local"))
        );
        // Special chars get scrubbed; the result must stay inside `base`.
        let scrubbed = pcap_dump_dir_for(base, "a/b").unwrap();
        assert!(scrubbed.starts_with(base));
        assert_eq!(scrubbed, PathBuf::from("/var/lib/dumps/a_b"));
    }

    #[test]
    fn pcap_dump_dir_for_rejects_dangerous_names() {
        let base = Path::new("/var/lib/dumps");
        assert_eq!(pcap_dump_dir_for(base, ""), None);
        assert_eq!(pcap_dump_dir_for(base, "."), None);
        assert_eq!(pcap_dump_dir_for(base, ".."), None);
    }

    #[test]
    fn parse_minute_label_round_trips() {
        for &key in &[0i64, 1, 60, 29_633_130, 100_000_000] {
            let label = minute_label(key);
            assert_eq!(parse_minute_label(&label), Some(key), "label = {label}");
        }
    }

    #[test]
    fn parse_minute_label_known_value() {
        assert_eq!(
            parse_minute_label("20260505T1330"),
            Some(1_777_987_800 / 60)
        );
        assert_eq!(parse_minute_label("19700101T0000"), Some(0));
    }

    #[test]
    fn parse_minute_label_rejects_bad_input() {
        assert_eq!(parse_minute_label(""), None);
        assert_eq!(parse_minute_label("abc"), None);
        assert_eq!(parse_minute_label("2026-05-05T13:30"), None);
        assert_eq!(parse_minute_label("20260505T133"), None); // too short
        assert_eq!(parse_minute_label("20260505T13300"), None); // too long
        assert_eq!(parse_minute_label("20261305T1330"), None); // month 13
        assert_eq!(parse_minute_label("20260532T1330"), None); // day 32
        assert_eq!(parse_minute_label("20260505T2530"), None); // hour 25
        assert_eq!(parse_minute_label("20260505T1360"), None); // minute 60
        assert_eq!(parse_minute_label("20260505X1330"), None); // missing T
    }

    #[test]
    fn rotate_on_minute_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        // 2026-05-05 13:30:59 → minute_key 29633130, 13:31:01 → minute_key 29633131
        let t0 = 1_777_987_859_000_000i64;
        let t1 = 1_777_987_861_000_000i64;
        d.write(&make_pkt("s", t0, &[0xa1]));
        d.write(&make_pkt("s", t1, &[0xb1]));
        drop(d);

        let f0 = dir.path().join("s/20260505T1330.pcap");
        let f1 = dir.path().join("s/20260505T1331.pcap");
        assert!(f0.is_file(), "expected {}", f0.display());
        assert!(f1.is_file(), "expected {}", f1.display());
        let (_, p0) = read_pcap(&f0);
        let (_, p1) = read_pcap(&f1);
        assert_eq!(p0.len(), 1);
        assert_eq!(p1.len(), 1);
        assert_eq!(p0[0].1, vec![0xa1]);
        assert_eq!(p1[0].1, vec![0xb1]);
    }

    #[test]
    fn late_packet_writes_to_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        // Advance to 13:31, then a late 13:30 packet arrives (2026-05-05 UTC).
        let t_now = 1_777_987_861_000_000i64; // 13:31:01
        let t_late = 1_777_987_859_000_000i64; // 13:30:59
        d.write(&make_pkt("s", t_now, &[0xaa]));
        d.write(&make_pkt("s", t_late, &[0xbb]));
        drop(d);

        // Only the 13:31 file exists; both packets are inside it.
        assert!(!dir.path().join("s/20260505T1330.pcap").exists());
        let (_, pkts) = read_pcap(&dir.path().join("s/20260505T1331.pcap"));
        assert_eq!(
            pkts.len(),
            2,
            "late packet must be written into current file"
        );
        // Record timestamps preserved (not rewritten).
        assert_eq!(pkts[0].0, t_now);
        assert_eq!(pkts[1].0, t_late);
    }

    #[test]
    fn snappy_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg_snappy(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("s", 1_000_000, &[0x10, 0x20, 0x30]));
        d.write(&make_pkt("s", 2_000_000, &[0x40, 0x50]));
        drop(d);

        let path = dir.path().join("s/19700101T0000.pcap.snappy");
        assert!(path.is_file(), "expected snappy file at {}", path.display());
        let (_, pkts) = read_snappy_pcap(&path);
        assert_eq!(pkts.len(), 2);
        assert_eq!(pkts[0], (1_000_000, vec![0x10, 0x20, 0x30]));
        assert_eq!(pkts[1], (2_000_000, vec![0x40, 0x50]));
    }

    #[test]
    fn per_source_directory_layout() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("alpha", 1_000_000, &[0x01]));
        d.write(&make_pkt("beta", 1_000_000, &[0x02]));
        drop(d);

        assert!(dir.path().join("alpha").is_dir());
        assert!(dir.path().join("beta").is_dir());
        assert!(dir.path().join("alpha/19700101T0000.pcap").is_file());
        assert!(dir.path().join("beta/19700101T0000.pcap").is_file());
        // Each source's dir contains only its own files.
        let alpha_entries: Vec<_> = std::fs::read_dir(dir.path().join("alpha"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(alpha_entries, vec!["19700101T0000.pcap".to_string()]);
    }

    #[test]
    fn flush_makes_data_visible_before_close() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("s", 1_000_000, &[0xab]));
        d.flush_all();

        // Read while the dumper is still alive.
        let (_, pkts) = read_pcap(&dir.path().join("s/19700101T0000.pcap"));
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].1, vec![0xab]);

        drop(d);
    }

    /// Regression guard for the C1 fix: snappy mode must cascade flushes
    /// through both the FrameEncoder source buffer AND the underlying
    /// BufWriter. Under the old `Box<dyn Write>` approach, calling
    /// `flush()` only drained the FrameEncoder into the BufWriter, so the
    /// bytes never reached the file until drop. This test reads the file
    /// while the dumper is still alive, which would fail on the old code.
    #[test]
    fn flush_makes_data_visible_before_close_snappy() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg_snappy(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("s", 1_000_000, &[0xcd, 0xef]));
        d.flush_all();

        // Read while the dumper is still alive — must see the packet.
        let path = dir.path().join("s/19700101T0000.pcap.snappy");
        assert!(path.is_file(), "expected snappy file at {}", path.display());
        let (_, pkts) = read_snappy_pcap(&path);
        assert_eq!(
            pkts.len(),
            1,
            "flush must persist data through both layers under snappy"
        );
        assert_eq!(pkts[0], (1_000_000, vec![0xcd, 0xef]));

        drop(d);
    }

    #[test]
    fn link_type_pinned_per_source_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        // Two minutes, same source, same link_type → both files exist.
        d.write(&make_pkt("s", 1_000_000, &[0x01])); // minute 0
        d.write(&make_pkt("s", 61_000_000, &[0x02])); // minute 1

        // A bad packet on the same source should be dropped, not "reset" the pin.
        let mut bad = make_pkt("s", 62_000_000, &[0x03]);
        bad.link_type = 101;
        d.write(&bad);

        // Another good packet — file for minute 1 still grows.
        d.write(&make_pkt("s", 63_000_000, &[0x04]));
        drop(d);

        assert!(dir.path().join("s/19700101T0000.pcap").is_file());
        assert!(dir.path().join("s/19700101T0001.pcap").is_file());
        let (lt0, p0) = read_pcap(&dir.path().join("s/19700101T0000.pcap"));
        let (lt1, p1) = read_pcap(&dir.path().join("s/19700101T0001.pcap"));
        assert_eq!(lt0, 1);
        assert_eq!(lt1, 1);
        assert_eq!(p0.len(), 1);
        assert_eq!(
            p1.len(),
            2,
            "bad-link-type packet must not appear in any file"
        );
    }

    #[test]
    fn same_minute_restart_appends_not_truncates() {
        // Simulates a process restart within the same wall-clock minute:
        // the second dumper must append to the existing file, not wipe it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s/19700101T0000.pcap");

        // First "process": write one packet, close.
        {
            let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();
            d.write(&make_pkt("s", 1_000_000, &[0xa1]));
        }
        let size_after_first = std::fs::metadata(&path).unwrap().len();
        assert!(size_after_first > 0);

        // Second "process": same source, same minute. Must append.
        {
            let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();
            d.write(&make_pkt("s", 2_000_000, &[0xb2]));
        }

        let (_, pkts) = read_pcap(&path);
        assert_eq!(
            pkts.len(),
            2,
            "second packet must be appended, not truncated"
        );
        assert_eq!(pkts[0].1, vec![0xa1]);
        assert_eq!(pkts[1].1, vec![0xb2]);
        // Size must have grown (would shrink under truncation).
        let size_after_second = std::fs::metadata(&path).unwrap().len();
        assert!(size_after_second > size_after_first);
    }

    #[test]
    fn zero_byte_existing_minute_file_gets_global_header() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("s");
        std::fs::create_dir_all(&src_dir).unwrap();
        let path = src_dir.join("19700101T0000.pcap");
        std::fs::write(&path, []).unwrap();

        let mut file = PacketDumper::open_minute_file(&src_dir, 0, 1, PcapCompression::None)
            .expect("open minute file");
        file.sink.flush_all_layers().unwrap();
        drop(file);

        let (lt, pkts) = read_pcap(&path);
        assert_eq!(lt, 1);
        assert!(
            pkts.is_empty(),
            "newly recreated file should contain only the global header"
        );
    }

    #[test]
    fn recovers_when_source_dir_was_externally_removed() {
        // Once `PacketDumper` has cached a `SourceWriter`, it never
        // re-runs `create_dir_all` for that source. If retention (or an
        // operator) deletes an idle source's dir, the next packet must
        // still produce a usable file — `open_minute_file` recreates the
        // dir on ENOENT instead of disabling the source.
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("s");
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        // First packet at minute 0 — caches the SourceWriter.
        d.write(&make_pkt("s", 1_000_000, &[0xa1]));
        d.flush_all();
        assert!(src_dir.is_dir());

        // External removal: drain the dir and remove it.
        std::fs::remove_dir_all(&src_dir).unwrap();
        assert!(!src_dir.exists());

        // Second packet at minute 1 — must recover, write to a new file.
        d.write(&make_pkt("s", 61_000_000, &[0xb2]));
        d.flush_all();
        drop(d);

        let path = src_dir.join("19700101T0001.pcap");
        assert!(
            path.is_file(),
            "expected recovered minute file at {}",
            path.display()
        );
        let (_, pkts) = read_pcap(&path);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].1, vec![0xb2]);
    }

    #[test]
    fn same_minute_restart_appends_snappy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s/19700101T0000.pcap.snappy");

        {
            let mut d = PacketDumper::new(cfg_snappy(dir.path()), test_metrics()).unwrap();
            d.write(&make_pkt("s", 1_000_000, &[0xa1, 0xa2, 0xa3]));
        }
        {
            let mut d = PacketDumper::new(cfg_snappy(dir.path()), test_metrics()).unwrap();
            d.write(&make_pkt("s", 2_000_000, &[0xb1]));
        }

        let (_, pkts) = read_snappy_pcap(&path);
        assert_eq!(
            pkts.len(),
            2,
            "snappy mode must also append across restarts"
        );
        assert_eq!(pkts[0].1, vec![0xa1, 0xa2, 0xa3]);
        assert_eq!(pkts[1].1, vec![0xb1]);
    }
}
