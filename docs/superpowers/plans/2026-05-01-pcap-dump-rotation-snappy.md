# pcap_dump rotation + snappy compression — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `pcap_dump`'s single-file-per-source layout with per-source directories, wall-clock minute file rotation, and optional snappy compression, while preserving every existing safety guarantee (per-source error isolation, link_type pinning, throttled error logging, capture pipeline never fails on dumper errors).

**Architecture:** Bypass `pcap::Savefile` and write the classic pcap format directly into a `BufWriter<File>` (or `snap::write::FrameEncoder<BufWriter<File>>` when compression is `snappy`). Each `SourceWriter` lazily creates a per-source subdirectory and holds at most one open `MinuteFile`. Out-of-order packets ratchet forward and are written into the current file with a `CaptureDumpLateMinutePackets` counter. Flush is driven by the existing 1 s heartbeat path — no new timer.

**Tech Stack:** Rust, `snap = "1"` (pure-Rust snappy framed encoder), existing `pcap` crate (only for tests reading files back), existing `ThrottledWarn`, existing `MetricsWorker`.

**Spec:** `docs/superpowers/specs/2026-05-01-pcap-dump-rotation-snappy-design.md`

---

## File Structure

| Path | Action | Responsibility |
|------|--------|----------------|
| `server/ts-common/src/internal_metrics.rs` | Modify | Register `CaptureDumpLateMinutePackets` counter |
| `server/ts-common/src/config.rs` | Modify | `PcapCompression` enum; `PcapDumpConfig` (drop `filename_template`, add `compression`); update tests |
| `server/ts-capture/Cargo.toml` | Modify | Add `snap = "1"` dep |
| `server/ts-capture/src/pcap_dump.rs` | Rewrite | New `SourceWriter` / `MinuteFile`; hand-written pcap encoder; rotation; snappy support; new + adapted tests |
| `server/ts-capture/src/pcap_live.rs` | Modify | Two `dumper.flush_all()` calls at heartbeat-emit sites |
| `server/app/tokenscope/src/main.rs` | Modify | Register the new metric on each capture worker |
| `server/config/default.toml` | Modify | Update commented `[pipeline.pcap_dump]` sample (drop `filename_template`, add `compression`) |

`server/ts-capture/src/pcap_file.rs` and `server/ts-capture/src/cloud_probe.rs` need **no** code changes. `cloud_probe.rs` already calls `flush_all()` on heartbeat; `pcap_file.rs` has no heartbeat (file replay is bounded — final flush on EOF is sufficient).

---

## Task 1: Register `CaptureDumpLateMinutePackets` counter

**Files:**
- Modify: `server/ts-common/src/internal_metrics.rs`
- Modify: `server/app/tokenscope/src/main.rs:414`

- [ ] **Step 1: Add the metric variant**

Open `server/ts-common/src/internal_metrics.rs` and locate the `-- Capture --` block in the `define_metrics!` invocation (around line 131-141). Insert one new line **after** `CaptureDumpErrors`:

```rust
    CaptureDumpErrors             => { kind: Counter, group: Capture,  short: "dump_errors"          },
    CaptureDumpLateMinutePackets  => { kind: Counter, group: Capture,  short: "dump_late_minute_pkts" },
```

- [ ] **Step 2: Register the metric on every capture worker**

In `server/app/tokenscope/src/main.rs`, find the `register_worker(&format!("capture.{j}"), ...)` call at lines 404-416 and append the new metric to the array (after `CaptureDumpErrors`):

```rust
                                Metric::CaptureDumpErrors,
                                Metric::CaptureDumpLateMinutePackets,
                            ],
```

- [ ] **Step 3: Verify compilation**

Run: `cd server && cargo check -p ts-common -p tokenscope`
Expected: build succeeds; no warnings about unused variants.

- [ ] **Step 4: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-common/src/internal_metrics.rs server/app/tokenscope/src/main.rs
git commit -m "feat(metrics): add CaptureDumpLateMinutePackets counter"
```

---

## Task 2: Add `snap` dependency

**Files:**
- Modify: `server/ts-capture/Cargo.toml`

- [ ] **Step 1: Add the dep**

In `server/ts-capture/Cargo.toml`, append `snap = "1"` to the `[dependencies]` block (alphabetically among the literal-version deps, which there are none today — put it at the bottom of `[dependencies]`):

```toml
[dependencies]
ts-common.workspace = true
pcap.workspace = true
libc.workspace = true
bytes.workspace = true
async-trait.workspace = true
zeromq.workspace = true
tokio.workspace = true
tracing.workspace = true
thiserror.workspace = true
tokio-util.workspace = true
snap = "1"
```

- [ ] **Step 2: Verify resolution**

Run: `cd server && cargo check -p ts-capture`
Expected: `snap v1.x.x` is fetched; build succeeds.

- [ ] **Step 3: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/Cargo.toml server/Cargo.lock
git commit -m "build(ts-capture): add snap crate for snappy framing"
```

---

## Task 3: Update `PcapDumpConfig` schema

**Files:**
- Modify: `server/ts-common/src/config.rs:622-647` (struct + defaults), `:1117-1131` and `:1397-1417` (tests)

- [ ] **Step 1: Replace the struct, add `PcapCompression` enum**

In `server/ts-common/src/config.rs`, replace the existing `PcapDumpConfig` block (lines 615-647 approximately) with:

```rust
/// Per-pipeline packet dump. When enabled, every non-heartbeat `RawPacket`
/// captured by this pipeline's sources is written to a Wireshark-openable
/// classic pcap file under `<dir>/<sanitized_source_id>/`. Files rotate on
/// wall-clock minute boundaries (by packet timestamp); empty minutes are
/// skipped. Optional snappy framed compression appends `.snappy` to the
/// filename. Off by default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PcapDumpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_pcap_dump_dir")]
    pub dir: String,
    #[serde(default)]
    pub compression: PcapCompression,
}

impl Default for PcapDumpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_pcap_dump_dir(),
            compression: PcapCompression::None,
        }
    }
}

fn default_pcap_dump_dir() -> String {
    "data/dumps".to_string()
}

/// Compression mode for pcap dump output. `None` writes plain `.pcap`;
/// `Snappy` writes snappy framed `.pcap.snappy` (decompress with `snzip
/// -d` or `snap::read::FrameDecoder` before opening in Wireshark).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PcapCompression {
    #[default]
    None,
    Snappy,
}
```

This drops `filename_template`, `default_pcap_dump_template`, and the related serde attribute.

- [ ] **Step 2: Update the existing config tests**

Find `pcap_dump_disabled_by_default` (around line 1117) and replace its assertions about `filename_template` with assertions about `compression`:

```rust
    #[test]
    fn pcap_dump_disabled_by_default() {
        let toml = r#"
            [[pipeline]]
            name = "p"

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert!(!cfg.pipelines[0].pcap_dump.enabled);
        assert_eq!(cfg.pipelines[0].pcap_dump.dir, "data/dumps");
        assert_eq!(
            cfg.pipelines[0].pcap_dump.compression,
            crate::config::PcapCompression::None,
        );
    }
```

Find `pcap_dump_parses_full_block` (around line 1397) and update it to use `compression`:

```rust
    #[test]
    fn pcap_dump_parses_full_block() {
        let toml = r#"
            [[pipeline]]
            name = "p"

            [pipeline.pcap_dump]
            enabled = true
            dir = "/tmp/dumps"
            compression = "snappy"

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let d = &cfg.pipelines[0].pcap_dump;
        assert!(d.enabled);
        assert_eq!(d.dir, "/tmp/dumps");
        assert_eq!(d.compression, crate::config::PcapCompression::Snappy);
    }
```

- [ ] **Step 3: Verify**

Run: `cd server && cargo test -p ts-common pcap_dump`
Expected: both `pcap_dump_disabled_by_default` and `pcap_dump_parses_full_block` pass.

- [ ] **Step 4: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-common/src/config.rs
git commit -m "feat(config): replace pcap_dump filename_template with compression mode

Adds PcapCompression { None | Snappy } and drops filename_template
(path is now fixed: <dir>/<source>/YYYYMMDDTHHMM.pcap[.snappy])."
```

---

## Task 4: Rewrite `pcap_dump.rs` — module skeleton + pcap format helpers

**Files:**
- Modify: `server/ts-capture/src/pcap_dump.rs` (replace module-level types and helpers; keep `sanitize` and `current_iso_basic` / `days_to_ymd`).

This task introduces the new types and the hand-written pcap encoder, but leaves the public `PacketDumper::write` behavior temporarily unchanged so tests compile. The next task wires up the rotation logic.

- [ ] **Step 1: Replace the imports and module doc**

Replace the top of `server/ts-capture/src/pcap_dump.rs` (through the imports) with:

```rust
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
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

use snap::write::FrameEncoder;

use ts_common::config::{PcapCompression, PcapDumpConfig};
use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_common::throttle::ThrottledWarn;

use crate::packet::RawPacket;

const WARN_THROTTLE: Duration = Duration::from_secs(5);
const BUF_CAPACITY: usize = 64 * 1024;
const PCAP_MAGIC: u32 = 0xa1b2_c3d4;
const PCAP_VERSION_MAJOR: u16 = 2;
const PCAP_VERSION_MINOR: u16 = 4;
const PCAP_SNAPLEN: u32 = 65535;
const MICROS_PER_MINUTE: i64 = 60 * 1_000_000;
```

- [ ] **Step 2: Replace `PacketDumperConfig`**

Right after the constants, replace the existing `PacketDumperConfig` block with:

```rust
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
```

- [ ] **Step 3: Add the hand-written pcap encoder helpers**

Add these private helpers (place them just above the `#[cfg(test)] mod tests` block, replacing any old helper code below `PacketDumper`'s impl):

```rust
/// Write the 24-byte classic-pcap global header (microsecond magic).
fn write_pcap_global_header<W: Write>(w: &mut W, link_type: u32) -> io::Result<()> {
    w.write_all(&PCAP_MAGIC.to_le_bytes())?;
    w.write_all(&PCAP_VERSION_MAJOR.to_le_bytes())?;
    w.write_all(&PCAP_VERSION_MINOR.to_le_bytes())?;
    w.write_all(&0i32.to_le_bytes())?;            // thiszone
    w.write_all(&0u32.to_le_bytes())?;            // sigfigs
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
```

- [ ] **Step 4: Remove the now-unused old imports**

In the same file, the old code imported `pcap::{Capture, Linktype, Packet, PacketHeader, Savefile}` for `Savefile`-based writing. After this rewrite the production module no longer needs them — they should be **absent** from the import block shown in Step 1 (verify they are gone). Tests in Task 5 will reintroduce `pcap::Capture` only inside the `#[cfg(test)]` module for reading files back.

**Do not run `cargo check` yet.** The file is intentionally in a half-rewritten state until Task 5 lands. Both tasks land as a single commit at the end of Task 5.

---

## Task 5: Rewrite `pcap_dump.rs` — `SourceWriter`, `MinuteFile`, rotation, snappy

**Files:**
- Modify: `server/ts-capture/src/pcap_dump.rs` (replace `PacketDumper` impl and `SourceWriter`)

- [ ] **Step 1: Replace the `PacketDumper` and `SourceWriter` types**

Replace the existing `PacketDumper` struct, `SourceWriter` struct, and `impl PacketDumper { ... }` block with:

```rust
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
    sink: Box<dyn Write + Send>,
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
                (source.src_dir.clone(), source.link_type, self.cfg.compression)
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
            let cur = source.current.as_mut().expect("just opened or pre-existing");
            let is_late = pkt_minute < cur.minute_key;
            let write_result = write_pcap_record(&mut cur.sink, pkt);
            (write_result, is_late)
        };

        if is_late {
            self.metrics.counter(Metric::CaptureDumpLateMinutePackets).inc();
        }

        if let Err(e) = write_result {
            let sid = pkt.source_id.clone();
            self.note_error(&format!(
                "pcap-dump: write failed for source '{sid}': {e}"
            ));
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
                if let Err(e) = cur.sink.flush() {
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

    /// Open `<src_dir>/<minute>.pcap[.snappy]` and write the global header.
    /// Returns the boxed `Write` (BufWriter or FrameEncoder<BufWriter>).
    fn open_minute_file(
        src_dir: &std::path::Path,
        minute_key: i64,
        link_type: u32,
        compression: PcapCompression,
    ) -> io::Result<MinuteFile> {
        let filename = format!("{}.pcap{}", minute_label(minute_key), match compression {
            PcapCompression::None => "",
            PcapCompression::Snappy => ".snappy",
        });
        let path = src_dir.join(&filename);
        let file = File::create(&path)?;
        let buf = BufWriter::with_capacity(BUF_CAPACITY, file);
        let mut sink: Box<dyn Write + Send> = match compression {
            PcapCompression::None => Box::new(buf),
            PcapCompression::Snappy => Box::new(FrameEncoder::new(buf)),
        };
        write_pcap_global_header(&mut sink, link_type)?;
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
```

- [ ] **Step 2: Replace the filename helpers**

Delete the existing `render_template`, `current_iso_basic` functions. Keep `days_to_ymd` and `sanitize` unchanged. Add a new `minute_label`:

```rust
/// Compact UTC label for a wall-clock minute. `minute_key` is
/// `pkt.timestamp_us / 60_000_000`. Returns e.g. `"20260501T1530"`.
fn minute_label(minute_key: i64) -> String {
    let total_secs = minute_key * 60;
    let days = total_secs.div_euclid(86_400);
    let rem = total_secs.rem_euclid(86_400);
    let (h, m) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32);
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}")
}
```

- [ ] **Step 3: Replace the entire `#[cfg(test)] mod tests` block**

Replace the whole `mod tests` block at the bottom of `pcap_dump.rs`. The replacement contains: imports, `test_metrics`, `make_pkt`, `cfg`, `cfg_snappy`, `read_pcap`, `read_snappy_pcap`, all adapted existing tests, and two encoder unit tests. Step 4 below adds the new feature tests on top of this.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pcap::Capture;
    use std::path::Path;
    use ts_common::internal_metrics::MetricsSystem;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        sys.register_worker(
            "test",
            &[Metric::CaptureDumpErrors, Metric::CaptureDumpLateMinutePackets],
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
        // 2026-05-01 15:30 UTC → seconds since epoch = 1777987800; minute_key = 29633130
        let key = 1_777_987_800 / 60;
        assert_eq!(minute_label(key), "20260501T1530");
    }
}
```

The unused `start_iso_template_produces_timestamped_filename` test from the old file is **removed** (the template feature is gone). All other existing tests are present in updated form above.

- [ ] **Step 4: Add new tests for rotation, late-minute, snappy, layout, flush, link-type-across-files**

Insert these test functions inside `mod tests`, **immediately before the closing `}`** that the previous step added:

```rust
    #[test]
    fn rotate_on_minute_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        // 15:30:59 → minute_key 29633130, 15:31:00 → minute_key 29633131
        let t0 = 1_777_987_859_000_000i64;
        let t1 = 1_777_987_861_000_000i64;
        d.write(&make_pkt("s", t0, &[0xa1]));
        d.write(&make_pkt("s", t1, &[0xb1]));
        drop(d);

        let f0 = dir.path().join("s/20260501T1530.pcap");
        let f1 = dir.path().join("s/20260501T1531.pcap");
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

        // Advance to 15:31, then a late 15:30 packet arrives.
        let t_now = 1_777_987_861_000_000i64;   // 15:31:01
        let t_late = 1_777_987_859_000_000i64;  // 15:30:59
        d.write(&make_pkt("s", t_now, &[0xaa]));
        d.write(&make_pkt("s", t_late, &[0xbb]));
        drop(d);

        // Only the 15:31 file exists; both packets are inside it.
        assert!(!dir.path().join("s/20260501T1530.pcap").exists());
        let (_, pkts) = read_pcap(&dir.path().join("s/20260501T1531.pcap"));
        assert_eq!(pkts.len(), 2, "late packet must be written into current file");
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

    #[test]
    fn link_type_pinned_per_source_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = PacketDumper::new(cfg(dir.path()), test_metrics()).unwrap();

        // Two minutes, same source, same link_type → both files exist.
        d.write(&make_pkt("s", 1_000_000, &[0x01]));     // minute 0
        d.write(&make_pkt("s", 61_000_000, &[0x02]));    // minute 1

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
        assert_eq!(p1.len(), 2, "bad-link-type packet must not appear in any file");
    }
```

- [ ] **Step 5: Run the full ts-capture test suite**

Run: `cd server && cargo test -p ts-capture pcap_dump -- --nocapture`
Expected: all tests pass — both adapted and new ones.

- [ ] **Step 6: Run the workspace check**

Run: `cd server && cargo check --workspace`
Expected: no errors anywhere (including main.rs and other consumers of `PacketDumperConfig`).

- [ ] **Step 7: Commit Tasks 4 and 5 together**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/src/pcap_dump.rs
git commit -m "feat(pcap-dump): per-source dirs, minute rotation, snappy support

Replaces single-file-per-source layout with <dir>/<source>/<minute>.pcap[.snappy].
Bypasses libpcap's Savefile to write the classic pcap format directly,
unifying the compressed and uncompressed paths. Late-minute (out-of-order
across minute boundary) packets ratchet forward and are counted in
CaptureDumpLateMinutePackets. Flush is driven by the existing 1s
heartbeat — no new timer."
```

---

## Task 6: Wire heartbeat-driven flush in `pcap_live.rs`

**Files:**
- Modify: `server/ts-capture/src/pcap_live.rs:199-207` (real-packet HB path), `:256-268` (idle TimeoutExpired HB path)

- [ ] **Step 1: Add `flush_all()` at the real-packet heartbeat emit site**

Locate the block in `pcap_live.rs` around line 197:

```rust
                        if last_hb_ts == 0 {
                            last_hb_ts = raw.timestamp_us;
                        } else if raw.timestamp_us - last_hb_ts >= HEARTBEAT_INTERVAL_US {
                            let hb = RawPacket::heartbeat(raw.timestamp_us, source_id.clone());
                            if tx.blocking_send(hb).is_err() {
                                tracing::debug!("pcap-live: channel closed, stopping");
                                break;
                            }
                            metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                            last_hb_ts = raw.timestamp_us;
                        }
```

Add a `flush_all()` call inside the `else if` branch, after the metric counter:

```rust
                        if last_hb_ts == 0 {
                            last_hb_ts = raw.timestamp_us;
                        } else if raw.timestamp_us - last_hb_ts >= HEARTBEAT_INTERVAL_US {
                            let hb = RawPacket::heartbeat(raw.timestamp_us, source_id.clone());
                            if tx.blocking_send(hb).is_err() {
                                tracing::debug!("pcap-live: channel closed, stopping");
                                break;
                            }
                            metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                            last_hb_ts = raw.timestamp_us;
                            // Flush dump buffers on each heartbeat so a hard
                            // termination loses at most ~1s of buffered data.
                            if let Some(d) = dumper.as_mut() {
                                d.flush_all();
                            }
                        }
```

- [ ] **Step 2: Add `flush_all()` at the idle (TimeoutExpired) heartbeat emit site**

Locate the block around line 256:

```rust
                        if last_hb_ts > 0 {
                            let wall_us = wall_clock_us();
                            if wall_us - last_hb_ts >= HEARTBEAT_INTERVAL_US {
                                let hb_ts = (wall_us - SAFETY_MARGIN_US).max(last_hb_ts + 1);
                                let hb = RawPacket::heartbeat(hb_ts, source_id.clone());
                                if tx.blocking_send(hb).is_err() {
                                    tracing::debug!("pcap-live: channel closed, stopping");
                                    break;
                                }
                                metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                                last_hb_ts = hb_ts;
                            }
                        }
```

Add the same `flush_all()` call after the counter:

```rust
                        if last_hb_ts > 0 {
                            let wall_us = wall_clock_us();
                            if wall_us - last_hb_ts >= HEARTBEAT_INTERVAL_US {
                                let hb_ts = (wall_us - SAFETY_MARGIN_US).max(last_hb_ts + 1);
                                let hb = RawPacket::heartbeat(hb_ts, source_id.clone());
                                if tx.blocking_send(hb).is_err() {
                                    tracing::debug!("pcap-live: channel closed, stopping");
                                    break;
                                }
                                metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                                last_hb_ts = hb_ts;
                                // Flush dump buffers on each idle heartbeat too.
                                if let Some(d) = dumper.as_mut() {
                                    d.flush_all();
                                }
                            }
                        }
```

- [ ] **Step 3: Verify**

Run: `cd server && cargo check -p ts-capture && cargo test -p ts-capture`
Expected: build succeeds; all tests pass (no test-level coverage for this — runtime-only).

- [ ] **Step 4: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/src/pcap_live.rs
git commit -m "feat(pcap-live): flush pcap_dump buffers on every heartbeat

Bounds dump-buffer crash-loss to ~1s by piggybacking on the existing
1s heartbeat (real-packet and idle TimeoutExpired branches both flush)."
```

---

## Task 7: Update `default.toml` sample

**Files:**
- Modify: `server/config/default.toml:53-56`

- [ ] **Step 1: Replace the commented sample**

Replace the existing `[pipeline.pcap_dump]` block (around lines 53-56) with:

```toml
# # Per-source directory layout: <dir>/<sanitized_source_id>/<minute>.pcap[.snappy]
# # Files rotate on wall-clock minute (by packet timestamp). Empty minutes
# # are skipped. compression="snappy" appends .snappy and writes via the
# # snap framed format (decompress with `snzip -d` before opening in Wireshark).
# [pipeline.pcap_dump]
# enabled = false
# dir = "data/dumps/local"
# compression = "none"   # "none" | "snappy"
```

- [ ] **Step 2: Verify config still parses**

Run: `cd server && cargo test -p ts-common -- --nocapture pcap_dump`
Expected: pass.

- [ ] **Step 3: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/config/default.toml
git commit -m "docs(config): document per-source dirs, minute rotation, snappy"
```

---

## Task 8: Full workspace verification

**Files:** none (validation only).

- [ ] **Step 1: Workspace build**

Run: `cd server && cargo build --workspace`
Expected: clean build, no warnings about unused imports / variants.

- [ ] **Step 2: Workspace tests**

Run: `cd server && cargo test --workspace`
Expected: all tests pass. Pay special attention to: `ts-common` config tests, `ts-capture::pcap_dump::tests::*` (all of them), `ts-capture::pcap_live::tests::*`.

- [ ] **Step 3: Smoke test with a real pcap file**

If a sample pcap is available at `data/captures/sample.pcap`, run a short smoke test by setting `[pipeline.pcap_dump] enabled = true` in a local config and replaying the sample. Verify:
- `data/dumps/<source>/<minute>.pcap` files appear, sizes increase, then plateau when capture ends.
- Each file opens cleanly in Wireshark.
- Switching `compression = "snappy"` produces `.pcap.snappy` files; `snzip -d` then Wireshark works.

If no sample pcap is available, this step is "best effort" — note in the commit that smoke testing was deferred.

- [ ] **Step 4: Update the spec status**

Edit `docs/superpowers/specs/2026-05-01-pcap-dump-rotation-snappy-design.md` line 3 to mark implementation done:

```markdown
**Status:** Implemented (commit <hash>)
```

Replace `<hash>` with the merge commit's short SHA (or the last commit's SHA if landed directly to `main`).

- [ ] **Step 5: Final commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add docs/superpowers/specs/2026-05-01-pcap-dump-rotation-snappy-design.md
git commit -m "docs(pcap-dump-spec): mark implemented"
```

---

## Self-review notes

- **Spec coverage:** every section of the spec maps to a task — file layout (T5), writer pipeline (T4-T5), rotation (T5), out-of-order (T5), flush (T5+T6), config (T3), errors+metrics (T1+T5), dependencies (T2), tests (T5), default.toml sample (T7).
- **Type / name consistency:** `PcapCompression`, `PacketDumperConfig.compression`, `MinuteFile.minute_key`, `Metric::CaptureDumpLateMinutePackets` are referenced consistently across tasks.
- **Late-minute counter test:** `late_packet_writes_to_current_file` reads the metric implicitly through file content; if a stronger assertion is needed, the test can be extended to query `MetricsSystem` for the counter value (the existing test pattern doesn't currently expose this — leave for follow-up if it surfaces).
- **`days_to_ymd` reuse:** `minute_label` reuses the existing helper; the test `days_to_ymd_known_dates` continues to gate it.
- **No placeholders:** every code block is concrete; no "TBD", "similar to above", or "add error handling".
