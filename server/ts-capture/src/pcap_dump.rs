//! Optional packet dump to pcap files.
//!
//! Writes every non-heartbeat [`RawPacket`] a source produces to a
//! Wireshark-openable classic pcap file, grouped by `source_id` (one file per
//! source, lazily created on first packet). Disabled by default; enabled per
//! pipeline via `[pipeline.pcap_dump]`. Dump failures are logged and the
//! offending source is muted — capture never fails on behalf of the dumper.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pcap::{Capture, Linktype, Packet, PacketHeader, Savefile};

use ts_common::config::PcapDumpConfig;
use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_common::throttle::ThrottledWarn;

use crate::packet::RawPacket;

const WARN_THROTTLE: Duration = Duration::from_secs(5);

/// Writer config resolved from [`PcapDumpConfig`].
#[derive(Debug, Clone)]
pub struct PacketDumperConfig {
    pub dir: PathBuf,
    pub filename_template: String,
}

impl PacketDumperConfig {
    pub fn from_config(cfg: &PcapDumpConfig) -> Self {
        Self {
            dir: PathBuf::from(&cfg.dir),
            filename_template: cfg.filename_template.clone(),
        }
    }
}

/// Lazily opens one [`Savefile`] per `source_id`, each pinned to the link type
/// of the first packet seen on that source. Packets whose link type disagrees
/// with the pinned value are dropped (throttled warn).
pub struct PacketDumper {
    cfg: PacketDumperConfig,
    writers: HashMap<String, SourceWriter>,
    disabled: HashSet<String>,
    err_throttle: ThrottledWarn,
    metrics: MetricsWorker,
    start_iso: String,
}

struct SourceWriter {
    file: Savefile,
    link_type: u32,
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
            start_iso: current_iso_basic(),
        })
    }

    /// Write one packet. Heartbeats are silently skipped. All I/O errors are
    /// swallowed — the offending source is muted so capture can continue.
    pub fn write(&mut self, pkt: &RawPacket) {
        if pkt.is_heartbeat() {
            return;
        }
        if self.disabled.contains(&pkt.source_id) {
            return;
        }

        if !self.writers.contains_key(&pkt.source_id) {
            match self.open_source(pkt) {
                Some(w) => {
                    self.writers.insert(pkt.source_id.clone(), w);
                }
                None => return, // open_source already logged + disabled source
            }
        }

        let entry = self.writers.get_mut(&pkt.source_id).expect("just inserted");

        if entry.link_type != pkt.link_type {
            let pinned = entry.link_type;
            let got = pkt.link_type;
            let sid = pkt.source_id.clone();
            self.note_error(&format!(
                "pcap-dump: link_type mismatch on source '{sid}' (pinned={pinned}, got={got}); dropping packet"
            ));
            return;
        }

        let header = PacketHeader {
            ts: libc::timeval {
                tv_sec: (pkt.timestamp_us / 1_000_000) as libc::time_t,
                tv_usec: (pkt.timestamp_us % 1_000_000) as libc::suseconds_t,
            },
            caplen: pkt.caplen,
            len: pkt.wirelen,
        };
        let packet = Packet::new(&header, &pkt.data);
        // Savefile::write is infallible at the Rust level (pcap_dump returns
        // no status); failures surface on flush/close via pcap_dump_close.
        entry.file.write(&packet);
    }

    /// Flush every open source's stdio buffer to the kernel. Cheap; safe to
    /// call from the shutdown path to bound data loss on hard termination.
    /// Flush errors are logged+counted like any other dump error but never
    /// propagated.
    pub fn flush_all(&mut self) {
        let mut errs: Vec<(String, String)> = Vec::new();
        for (sid, w) in self.writers.iter_mut() {
            if let Err(e) = w.file.flush() {
                errs.push((sid.clone(), e.to_string()));
            }
        }
        for (sid, e) in errs {
            self.note_error(&format!("pcap-dump: flush failed for source '{sid}': {e}"));
        }
    }

    fn open_source(&mut self, pkt: &RawPacket) -> Option<SourceWriter> {
        let path = match self.resolve_path(&pkt.source_id) {
            Ok(p) => p,
            Err(e) => {
                let sid = pkt.source_id.clone();
                self.note_error(&format!("pcap-dump: refusing to open source '{sid}': {e}"));
                self.disabled.insert(sid);
                return None;
            }
        };

        let lt = pkt.link_type as i32;
        let cap = match Capture::dead(Linktype(lt)) {
            Ok(c) => c,
            Err(e) => {
                let sid = pkt.source_id.clone();
                self.note_error(&format!(
                    "pcap-dump: pcap_open_dead failed for source '{sid}' (link_type={lt}): {e}"
                ));
                self.disabled.insert(sid);
                return None;
            }
        };
        let file = match cap.savefile(&path) {
            Ok(f) => f,
            Err(e) => {
                let sid = pkt.source_id.clone();
                let p = path.display().to_string();
                self.note_error(&format!(
                    "pcap-dump: failed to open '{p}' for source '{sid}': {e}"
                ));
                self.disabled.insert(sid);
                return None;
            }
        };

        tracing::info!(
            "pcap-dump: writing source '{}' → {} (link_type={})",
            pkt.source_id,
            path.display(),
            pkt.link_type,
        );

        Some(SourceWriter {
            file,
            link_type: pkt.link_type,
        })
    }

    fn resolve_path(&self, source_id: &str) -> Result<PathBuf, String> {
        let safe = sanitize(source_id).ok_or_else(|| format!("invalid source_id '{source_id}'"))?;
        let filename = render_template(&self.cfg.filename_template, &safe, &self.start_iso);
        let out = self.cfg.dir.join(&filename);
        // Defence-in-depth: even if a template or sanitization bug lets a
        // traversal slip through, refuse any path that escapes the dir.
        if !out.starts_with(&self.cfg.dir) {
            return Err(format!(
                "resolved path '{}' escapes dump dir",
                out.display()
            ));
        }
        Ok(out)
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

/// Replace any byte outside `[A-Za-z0-9._-]` with `_`. Rejects empty, `.`,
/// and `..` entirely — those would create ambiguous or traversal-prone paths.
fn sanitize(source_id: &str) -> Option<String> {
    if source_id.is_empty() {
        return None;
    }
    let out: String = source_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out == "." || out == ".." {
        return None;
    }
    Some(out)
}

fn render_template(template: &str, source_id: &str, start_iso: &str) -> String {
    template
        .replace("{source_id}", source_id)
        .replace("{start_iso}", start_iso)
}

/// Compact UTC timestamp like `20260420T153000Z`, derived once at dumper
/// construction. Used for restart-safe filenames via `{start_iso}`.
fn current_iso_basic() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Break secs into date/time components manually to avoid pulling chrono
    // into ts-capture for a single timestamp.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = days_to_ymd(days as i64);
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}{s:02}Z")
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::path::Path;
    use ts_common::internal_metrics::MetricsSystem;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        sys.register_worker("test", &[Metric::CaptureDumpErrors])
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
            filename_template: "{source_id}.pcap".to_string(),
        }
    }

    fn read_all(path: &Path) -> (u32, Vec<(i64, Vec<u8>)>) {
        let mut cap = Capture::from_file(path).expect("open pcap");
        let lt = cap.get_datalink().0 as u32;
        let mut out = Vec::new();
        while let Ok(p) = cap.next_packet() {
            let ts = p.header.ts.tv_sec as i64 * 1_000_000 + p.header.ts.tv_usec as i64;
            out.push((ts, p.data.to_vec()));
        }
        (lt, out)
    }

    #[test]
    fn round_trip_two_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("a", 1_000_000, &[0x01, 0x02, 0x03]));
        d.write(&make_pkt("b", 2_500_000, &[0xaa, 0xbb]));
        d.write(&make_pkt("a", 3_000_250, &[0x04]));
        drop(d);

        let (lt_a, pkts_a) = read_all(&dir.path().join("a.pcap"));
        assert_eq!(lt_a, 1);
        assert_eq!(pkts_a.len(), 2);
        assert_eq!(pkts_a[0], (1_000_000, vec![0x01, 0x02, 0x03]));
        assert_eq!(pkts_a[1], (3_000_250, vec![0x04]));

        let (_lt_b, pkts_b) = read_all(&dir.path().join("b.pcap"));
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

        let (_, pkts) = read_all(&dir.path().join("s.pcap"));
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

        let (_, pkts) = read_all(&dir.path().join("s.pcap"));
        assert_eq!(pkts.len(), 1, "mismatched link_type packet must be dropped");
    }

    #[test]
    fn source_id_is_sanitized_and_stays_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        d.write(&make_pkt("evil/../x", 1_000_000, &[0x01]));
        drop(d);

        let expected = dir.path().join("evil_.._x.pcap");
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

        // No file created for the empty id; dir is empty.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn start_iso_template_produces_timestamped_filename() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.filename_template = "{source_id}_{start_iso}.pcap".to_string();
        let mut d = PacketDumper::new(c, test_metrics()).unwrap();
        d.write(&make_pkt("s", 1_000_000, &[0x01]));
        drop(d);

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1);
        let name = &entries[0];
        assert!(name.starts_with("s_"));
        assert!(name.ends_with(".pcap"));
        // Basic ISO shape: 8 digits, 'T', 6 digits, 'Z'
        assert!(name.contains('T') && name.contains('Z'));
    }

    #[test]
    fn days_to_ymd_known_dates() {
        assert_eq!(super::days_to_ymd(0), (1970, 1, 1));
        assert_eq!(super::days_to_ymd(31), (1970, 2, 1));
        assert_eq!(super::days_to_ymd(365), (1971, 1, 1));
        // 1999-12-31 → 2000-01-01 boundary (leap-year / Y2K spot check).
        assert_eq!(super::days_to_ymd(10_957), (2000, 1, 1));
        assert_eq!(super::days_to_ymd(10_956), (1999, 12, 31));
    }
}
