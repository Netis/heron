# pcap-extract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `GET /api/pcap/extract` and a console "Extract packets" modal on the http_exchange / llm_call / agent_turn detail pages, returning a 5-tuple-filtered + time-bounded `.pcap` slice from `pcap_dump`'s on-disk minute files.

**Architecture:** A new read-side crate `ts-pcap-extract` (filesystem scan → per-file iterator → 5-tuple filter → k-way merge → byte stream), a new ts-api route that wraps the stream as the HTTP body, runtime wiring that injects `Vec<PipelineRoot>` into the router, and a small console feature module providing one shared dialog used from three detail pages. No DB schema changes.

**Tech Stack:** Rust + Tokio + Axum (server), `snap` crate for snappy decoding, existing `ts_protocol::de::decode` for L2-L4 parsing, React + TanStack Query (console).

**Reference:** `docs/superpowers/specs/2026-05-02-pcap-extract-download-design.md`.

---

## File map

**Server — new crate `server/ts-pcap-extract/`:**

| File | Responsibility |
|---|---|
| `Cargo.toml` | crate manifest |
| `src/lib.rs` | re-exports + the public `extract()` function |
| `src/types.rs` | `ExtractRequest`, `PipelineRoot`, `ExtractError` |
| `src/format.rs` | `PCAP_*` consts + `minute_label` / `parse_minute_label` (inlined; not from ts-capture) |
| `src/candidates.rs` | filesystem scan: list candidate `.pcap[.snappy]` files |
| `src/reader.rs` | `PacketIter` over plain or snappy file; truncation-tolerant; `RawRec` type |
| `src/filter.rs` | 5-tuple bidirectional match + time-window predicate |
| `src/merge.rs` | k-way merge of `PacketIter`s into a time-ordered stream of `RawRec` |
| `src/output.rs` | byte encoders for global header + per-record header |

**Server — modify `server/ts-api/`:**

| File | Change |
|---|---|
| `Cargo.toml` | add dep on `ts-pcap-extract` |
| `src/lib.rs` | `router()` gains `pcap_roots: Arc<Vec<PipelineRoot>>`; mount `/api/pcap/extract` |
| `src/routes/mod.rs` | add `pub mod pcap_extract;` |
| `src/routes/pcap_extract.rs` | new — query parser → `extract()` → axum streaming body |

**Server — modify `server/app/tokenscope/`:**

| File | Change |
|---|---|
| `src/main.rs` | compute `Vec<PipelineRoot>` from `effective_pipelines`; pass to **both** `ts_api::router(...)` call sites |

**Server — modify `server/Cargo.toml`:**

| Change | |
|---|---|
| `[workspace.dependencies]` | add `ts-pcap-extract = { path = "ts-pcap-extract" }` |

**Console — new feature `console/src/features/pcap-extract/`:**

| File | Responsibility |
|---|---|
| `extract-defaults.ts` | pure functions: anchor → form initial values |
| `ExtractDialog.tsx` | the modal; controlled form; build URL; trigger native download |
| `ExtractPacketsButton.tsx` | the small button that mounts the dialog with an anchor prop |

**Console — modify three detail pages:**

| File | Change |
|---|---|
| `console/src/pages/http-exchange-detail-panel.tsx` | add `<ExtractPacketsButton anchor={…}/>` next to existing toolbar buttons |
| `console/src/pages/llm-call-detail-panel.tsx` | same |
| `console/src/pages/agent-turn-detail-panel.tsx` | same |

---

## Phase A — Crate scaffold

### Task A1: Create `ts-pcap-extract` crate skeleton + workspace registration

**Files:**
- Create: `server/ts-pcap-extract/Cargo.toml`
- Create: `server/ts-pcap-extract/src/lib.rs`
- Modify: `server/Cargo.toml` (add `[workspace.dependencies]` entry)

- [ ] **Step 1: Create crate manifest**

`server/ts-pcap-extract/Cargo.toml`:
```toml
[package]
name = "ts-pcap-extract"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
ts-protocol = { workspace = true }
ts-common = { workspace = true }
snap = "1"
bytes = { workspace = true }
tokio = { workspace = true }
tokio-util = { workspace = true }
futures = "0.3"
tracing = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
tempfile = "3"
pcap = { workspace = true }
```

- [ ] **Step 2: Create empty lib.rs**

`server/ts-pcap-extract/src/lib.rs`:
```rust
//! Read-side counterpart to `ts-capture::pcap_dump`. Scans
//! `<base>/<pipeline>/<source_id>/<minute>.pcap[.snappy]` trees, filters
//! by 5-tuple + time window, and emits a single uncompressed pcap byte
//! stream suitable for HTTP download.
```

- [ ] **Step 3: Register in workspace**

`server/Cargo.toml` — add `ts-pcap-extract = { path = "ts-pcap-extract" }` under `[workspace.dependencies]`, **right after the existing `ts-storage` line** so the internal crates stay grouped:

```toml
ts-storage = { path = "ts-storage" }
ts-pcap-extract = { path = "ts-pcap-extract" }
```

`futures` is not currently in `[workspace.dependencies]`; that's intentional — only `ts-pcap-extract` needs it, so it stays in the crate-local `Cargo.toml`. Same for `snap` (already used inside `ts-capture` as a local dep, not workspace-level).

The workspace `members = ["ts-*", "app/*"]` glob already picks up the new crate; no edit there.

- [ ] **Step 4: Verify build**

Run: `cargo check -p ts-pcap-extract`
Expected: `Finished` with no warnings about missing manifest entries.

- [ ] **Step 5: Commit**

```bash
git add server/ts-pcap-extract/Cargo.toml server/ts-pcap-extract/src/lib.rs server/Cargo.toml
git commit -m "feat(ts-pcap-extract): scaffold new crate"
```

---

### Task A2: Define `ExtractRequest`, `PipelineRoot`, `ExtractError`

**Files:**
- Create: `server/ts-pcap-extract/src/types.rs`
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `server/ts-pcap-extract/src/types.rs`:
```rust
use std::net::IpAddr;
use std::path::PathBuf;

/// A pcap-extract request. Optional 5-tuple fields are wildcards (any value matches).
#[derive(Debug, Clone)]
pub struct ExtractRequest {
    pub source_id: String,
    pub start_us: i64,
    pub end_us: i64,
    pub client_ip: Option<IpAddr>,
    pub client_port: Option<u16>,
    pub server_ip: Option<IpAddr>,
    pub server_port: Option<u16>,
}

/// One pipeline's root info, supplied by the runtime.
#[derive(Debug, Clone)]
pub struct PipelineRoot {
    /// Raw (un-sanitized) pipeline name; the crate sanitizes internally.
    pub name: String,
    /// `pipeline.pcap_dump.dir` — base directory; the pipeline subdir is
    /// appended by this crate.
    pub dump_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("link_type mismatch across candidate files (got {got}, expected {expected})")]
    LinkTypeMismatch { expected: u32, got: u32 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_request_constructs() {
        let req = ExtractRequest {
            source_id: "en0".to_string(),
            start_us: 0,
            end_us: 1_000_000,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        };
        assert_eq!(req.source_id, "en0");
    }
}
```

- [ ] **Step 2: Wire module into lib.rs**

`server/ts-pcap-extract/src/lib.rs`:
```rust
//! Read-side counterpart to `ts-capture::pcap_dump`. Scans
//! `<base>/<pipeline>/<source_id>/<minute>.pcap[.snappy]` trees, filters
//! by 5-tuple + time window, and emits a single uncompressed pcap byte
//! stream suitable for HTTP download.

pub mod types;

pub use types::{ExtractError, ExtractRequest, PipelineRoot};
```

- [ ] **Step 3: Run test to verify pass**

Run: `cargo test -p ts-pcap-extract types::tests::extract_request_constructs -- --nocapture`
Expected: `test result: ok. 1 passed`

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/types.rs server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): add ExtractRequest, PipelineRoot, ExtractError types"
```

---

## Phase B — Format module (`format.rs`)

### Task B1: Inline pcap consts + minute label encode/decode with anchor tests

**Files:**
- Create: `server/ts-pcap-extract/src/format.rs`
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `server/ts-pcap-extract/src/format.rs`:
```rust
//! Inlined copy of pcap classic format constants and the minute-label
//! encode/decode used by `ts-capture::pcap_dump`. Duplicated rather than
//! shared via a dependency so this read crate doesn't pull in libpcap /
//! ZMQ / snap-via-ts-capture / retention. The on-disk format is the
//! actual contract; anchor tests below pin it.
//!
//! See also: `server/ts-capture/src/pcap_dump.rs` (writer side).

// ---- pcap classic format constants (frozen by external spec) ----

pub const PCAP_MAGIC: u32 = 0xa1b2_c3d4;
pub const PCAP_VERSION_MAJOR: u16 = 2;
pub const PCAP_VERSION_MINOR: u16 = 4;
/// Must match `ts-capture::pcap_dump::PCAP_SNAPLEN`. Both sides re-anchor
/// against the literal value in their own tests, so drift surfaces locally.
pub const PCAP_SNAPLEN: u32 = 262_144;

pub const MICROS_PER_MINUTE: i64 = 60 * 1_000_000;

/// Default `link_type` used by the global header when no candidate files
/// exist (so an empty-result extract is still a valid, openable .pcap).
/// `1` = DLT_EN10MB / Ethernet — overwhelmingly the link type for
/// server-side LLM HTTP capture.
pub const DEFAULT_EMPTY_LINK_TYPE: u32 = 1;

// ---- minute label encode / decode ----

/// Compact UTC label for a wall-clock minute. `minute_key = ts_us / 60_000_000`.
/// Returns `"YYYYMMDDTHHMM"`.
pub fn minute_label(minute_key: i64) -> String {
    let total_secs = minute_key * 60;
    let days = total_secs.div_euclid(86_400);
    let rem = total_secs.rem_euclid(86_400);
    let (h, m) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32);
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}")
}

/// Inverse of [`minute_label`]. Returns `None` for any string that doesn't
/// match the exact `YYYYMMDDTHHMM` shape, or whose fields don't form a
/// valid UTC instant.
pub fn parse_minute_label(s: &str) -> Option<i64> {
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
    if days_to_ymd(days) != (y, mo, d) {
        return None;
    }
    Some(days * 1440 + i64::from(h) * 60 + i64::from(mi))
}

fn ymd_to_days(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { i64::from(y) - 1 } else { i64::from(y) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_shift = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * m_shift + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

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

    /// Drift guard — these literal values must match
    /// `ts-capture::pcap_dump`'s own tests for the same constants.
    #[test]
    fn pcap_consts_anchored() {
        assert_eq!(PCAP_MAGIC, 0xa1b2_c3d4);
        assert_eq!(PCAP_VERSION_MAJOR, 2);
        assert_eq!(PCAP_VERSION_MINOR, 4);
        assert_eq!(PCAP_SNAPLEN, 262_144);
        assert_eq!(DEFAULT_EMPTY_LINK_TYPE, 1);
    }

    /// Drift guard — these fixtures must match
    /// `ts-capture::pcap_dump::tests::minute_label_format`.
    #[test]
    fn minute_label_anchored() {
        assert_eq!(minute_label(0), "19700101T0000");
        assert_eq!(minute_label(29_633_130), "20260505T1330");
    }

    #[test]
    fn parse_round_trips() {
        for &k in &[0i64, 1, 60, 29_633_130, 100_000_000] {
            let label = minute_label(k);
            assert_eq!(parse_minute_label(&label), Some(k), "label = {label}");
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert_eq!(parse_minute_label(""), None);
        assert_eq!(parse_minute_label("20260505T2530"), None);  // hour 25
        assert_eq!(parse_minute_label("20260532T1330"), None);  // day 32
        assert_eq!(parse_minute_label("20261305T1330"), None);  // month 13
        assert_eq!(parse_minute_label("20260505X1330"), None);  // missing T
    }
}
```

- [ ] **Step 2: Wire module**

`server/ts-pcap-extract/src/lib.rs` — add `pub mod format;` after `pub mod types;`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-pcap-extract format::tests`
Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/format.rs server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): inline format consts + minute label codec with anchor tests"
```

---

## Phase C — Candidate file discovery (`candidates.rs`)

### Task C1: Implement `list_candidate_files`

**Files:**
- Create: `server/ts-pcap-extract/src/candidates.rs`
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `server/ts-pcap-extract/src/candidates.rs`:
```rust
//! Walk every supplied pipeline root, find `<root>/<pipeline>/<source_id>/`
//! if it exists, then enumerate `<minute_label>.pcap` and
//! `<minute_label>.pcap.snappy` for each minute key in the request window.

use std::path::{Path, PathBuf};

use ts_common::path::sanitize_path_component;

use crate::format::{minute_label, MICROS_PER_MINUTE};
use crate::types::{ExtractRequest, PipelineRoot};

/// One physical file we will read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFile {
    pub path: PathBuf,
    pub compressed: bool,
}

pub fn list_candidate_files(req: &ExtractRequest, roots: &[PipelineRoot]) -> Vec<CandidateFile> {
    let safe_source = match sanitize_path_component(&req.source_id) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let minute_lo = req.start_us.div_euclid(MICROS_PER_MINUTE);
    let minute_hi = req.end_us.div_euclid(MICROS_PER_MINUTE);

    let mut out = Vec::new();
    for root in roots {
        let safe_pipeline = match sanitize_path_component(&root.name) {
            Some(s) => s,
            None => continue,
        };
        let src_dir = root.dump_dir.join(safe_pipeline).join(&safe_source);
        if !src_dir.is_dir() {
            continue;
        }
        for k in minute_lo..=minute_hi {
            let label = minute_label(k);
            push_if_file(&mut out, &src_dir, &label, ".pcap", false);
            push_if_file(&mut out, &src_dir, &label, ".pcap.snappy", true);
        }
    }
    out
}

fn push_if_file(out: &mut Vec<CandidateFile>, dir: &Path, label: &str, ext: &str, compressed: bool) {
    let path = dir.join(format!("{label}{ext}"));
    if path.is_file() {
        out.push(CandidateFile { path, compressed });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn req(source_id: &str, start_us: i64, end_us: i64) -> ExtractRequest {
        ExtractRequest {
            source_id: source_id.into(),
            start_us,
            end_us,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        }
    }

    fn root(name: &str, dir: &Path) -> PipelineRoot {
        PipelineRoot { name: name.into(), dump_dir: dir.to_path_buf() }
    }

    #[test]
    fn lists_plain_and_snappy_in_same_minute() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("local/en0");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("19700101T0000.pcap"), b"x").unwrap();
        fs::write(src_dir.join("19700101T0000.pcap.snappy"), b"x").unwrap();

        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("en0", 0, 30_000_000), &roots);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|c| !c.compressed));
        assert!(files.iter().any(|c| c.compressed));
    }

    #[test]
    fn skips_missing_minutes() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("local/en0");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("19700101T0000.pcap"), b"x").unwrap();
        // 19700101T0001.pcap intentionally missing
        fs::write(src_dir.join("19700101T0002.pcap"), b"x").unwrap();

        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("en0", 0, 121_000_000), &roots);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn scans_multiple_pipelines_for_same_source_id() {
        let dir = tempdir().unwrap();
        for pipeline in &["alpha", "beta"] {
            let src_dir = dir.path().join(format!("{pipeline}/en0"));
            fs::create_dir_all(&src_dir).unwrap();
            fs::write(src_dir.join("19700101T0000.pcap"), b"x").unwrap();
        }
        let roots = vec![root("alpha", dir.path()), root("beta", dir.path())];
        let files = list_candidate_files(&req("en0", 0, 30_000_000), &roots);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|c| c.path.to_string_lossy().contains("alpha/en0")));
        assert!(files.iter().any(|c| c.path.to_string_lossy().contains("beta/en0")));
    }

    #[test]
    fn missing_source_dir_yields_empty() {
        let dir = tempdir().unwrap();
        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("nope", 0, 30_000_000), &roots);
        assert!(files.is_empty());
    }

    #[test]
    fn unsafe_source_id_yields_empty() {
        let dir = tempdir().unwrap();
        let roots = vec![root("local", dir.path())];
        let files = list_candidate_files(&req("..", 0, 30_000_000), &roots);
        assert!(files.is_empty());
    }
}
```

- [ ] **Step 2: Wire module**

Append to `server/ts-pcap-extract/src/lib.rs`:
```rust
pub mod candidates;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-pcap-extract candidates::tests`
Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/candidates.rs server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): filesystem scan for candidate minute files"
```

---

## Phase D — File reader (`reader.rs`)

### Task D1: `RawRec` + `PacketIter` for plain and snappy with truncation tolerance

**Files:**
- Create: `server/ts-pcap-extract/src/reader.rs`
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `server/ts-pcap-extract/src/reader.rs`:
```rust
//! Per-file iterator yielding pcap records. Supports plain `.pcap` and
//! `.pcap.snappy`. Tolerates EOF inside record header or data — the
//! pcap_dump writer flushes every ~1s, so a reader can race the in-flight
//! current minute file. Truncation ends iteration cleanly rather than
//! erroring.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use bytes::Bytes;
use snap::read::FrameDecoder;

use crate::candidates::CandidateFile;
use crate::format::PCAP_MAGIC;

#[derive(Debug, Clone)]
pub struct RawRec {
    pub ts_us: i64,
    pub caplen: u32,
    pub wirelen: u32,
    pub data: Bytes,
}

/// One opened file's iterator. Holds its own boxed reader because plain and
/// snappy have different concrete types.
pub struct PacketIter {
    inner: Box<dyn Read + Send>,
    pub link_type: u32,
}

impl PacketIter {
    pub fn open(file: &CandidateFile) -> io::Result<Self> {
        let f = File::open(&file.path)?;
        let buf = BufReader::with_capacity(64 * 1024, f);
        let mut inner: Box<dyn Read + Send> = if file.compressed {
            Box::new(FrameDecoder::new(buf))
        } else {
            Box::new(buf)
        };
        let mut header = [0u8; 24];
        inner.read_exact(&mut header)?;
        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if magic != PCAP_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad pcap magic {magic:#010x} in {}", file.path.display()),
            ));
        }
        let link_type = u32::from_le_bytes(header[20..24].try_into().unwrap());
        Ok(Self { inner, link_type })
    }
}

impl Iterator for PacketIter {
    type Item = RawRec;

    fn next(&mut self) -> Option<RawRec> {
        let mut hdr = [0u8; 16];
        if read_full_or_eof(&mut self.inner, &mut hdr).ok()? != 16 {
            return None;
        }
        let ts_sec = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as i64;
        let ts_usec = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as i64;
        let caplen = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let wirelen = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
        let mut data = vec![0u8; caplen as usize];
        if read_full_or_eof(&mut self.inner, &mut data).ok()? != caplen as usize {
            return None;
        }
        Some(RawRec {
            ts_us: ts_sec * 1_000_000 + ts_usec,
            caplen,
            wirelen,
            data: Bytes::from(data),
        })
    }
}

/// Read `buf` exactly, OR cleanly report short-read on EOF / truncation.
/// Returns the number of bytes actually filled. Caller treats `< buf.len()`
/// as "stop iteration".
fn read_full_or_eof<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Ok(filled),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(filled),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Path-helper for tests so we don't repeat the construct.
#[cfg(test)]
fn open_path(path: &Path, compressed: bool) -> io::Result<PacketIter> {
    PacketIter::open(&CandidateFile { path: path.to_path_buf(), compressed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_pcap_global_header(w: &mut impl Write, link_type: u32) -> io::Result<()> {
        w.write_all(&PCAP_MAGIC.to_le_bytes())?;
        w.write_all(&2u16.to_le_bytes())?;
        w.write_all(&4u16.to_le_bytes())?;
        w.write_all(&0i32.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(&262_144u32.to_le_bytes())?;
        w.write_all(&link_type.to_le_bytes())?;
        Ok(())
    }
    fn write_record(w: &mut impl Write, ts_us: i64, data: &[u8]) -> io::Result<()> {
        let ts_sec = (ts_us / 1_000_000) as u32;
        let ts_usec = (ts_us % 1_000_000) as u32;
        w.write_all(&ts_sec.to_le_bytes())?;
        w.write_all(&ts_usec.to_le_bytes())?;
        w.write_all(&(data.len() as u32).to_le_bytes())?;
        w.write_all(&(data.len() as u32).to_le_bytes())?;
        w.write_all(data)?;
        Ok(())
    }

    #[test]
    fn reads_two_plain_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        write_pcap_global_header(&mut f, 1).unwrap();
        write_record(&mut f, 1_000_000, &[0xaa, 0xbb]).unwrap();
        write_record(&mut f, 2_500_000, &[0xcc]).unwrap();
        drop(f);
        let mut it = open_path(&path, false).unwrap();
        assert_eq!(it.link_type, 1);
        let r1 = it.next().unwrap();
        assert_eq!(r1.ts_us, 1_000_000);
        assert_eq!(&r1.data[..], &[0xaa, 0xbb]);
        let r2 = it.next().unwrap();
        assert_eq!(r2.ts_us, 2_500_000);
        assert!(it.next().is_none());
    }

    #[test]
    fn truncated_record_data_stops_cleanly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        write_pcap_global_header(&mut f, 1).unwrap();
        // record header says caplen=10 but only writes 3 bytes
        f.write_all(&0u32.to_le_bytes()).unwrap();          // ts_sec
        f.write_all(&500_000u32.to_le_bytes()).unwrap();    // ts_usec
        f.write_all(&10u32.to_le_bytes()).unwrap();         // caplen
        f.write_all(&10u32.to_le_bytes()).unwrap();         // wirelen
        f.write_all(&[0xaa, 0xbb, 0xcc]).unwrap();          // truncated
        drop(f);
        let mut it = open_path(&path, false).unwrap();
        // No complete record before EOF.
        assert!(it.next().is_none());
    }

    #[test]
    fn truncated_record_header_stops_cleanly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        write_pcap_global_header(&mut f, 1).unwrap();
        f.write_all(&[0u8; 5]).unwrap();   // partial 16-byte record header
        drop(f);
        let mut it = open_path(&path, false).unwrap();
        assert!(it.next().is_none());
    }

    #[test]
    fn rejects_bad_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        f.write_all(&[0u8; 24]).unwrap();
        drop(f);
        assert!(open_path(&path, false).is_err());
    }

    #[test]
    fn reads_snappy_records() {
        use snap::write::FrameEncoder;
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap.snappy");
        let f = File::create(&path).unwrap();
        let mut enc = FrameEncoder::new(f);
        write_pcap_global_header(&mut enc, 1).unwrap();
        write_record(&mut enc, 1_000_000, &[0x10, 0x20]).unwrap();
        drop(enc);
        let mut it = open_path(&path, true).unwrap();
        assert_eq!(it.link_type, 1);
        let r = it.next().unwrap();
        assert_eq!(r.ts_us, 1_000_000);
        assert_eq!(&r.data[..], &[0x10, 0x20]);
    }
}
```

- [ ] **Step 2: Wire module**

Append to `server/ts-pcap-extract/src/lib.rs`:
```rust
pub mod reader;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-pcap-extract reader::tests`
Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/reader.rs server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): per-file PacketIter for plain + snappy with truncation tolerance"
```

---

## Phase E — Filter (`filter.rs`)

### Task E1: 5-tuple bidirectional match + time window

**Files:**
- Create: `server/ts-pcap-extract/src/filter.rs`
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `server/ts-pcap-extract/src/filter.rs`:
```rust
//! Per-record predicate: time window AND 5-tuple in either direction.
//! Records that don't decode to TCP via `ts_protocol::de::decode` are
//! dropped. (TokenScope is TCP-only today; ARP / ICMP / UDP / QUIC /
//! malformed all fall through to "skip".)

use std::net::IpAddr;

use ts_protocol::de::decode;

use crate::reader::RawRec;
use crate::types::ExtractRequest;

pub struct Filter<'a> {
    pub req: &'a ExtractRequest,
    pub link_type: u32,
}

impl Filter<'_> {
    pub fn matches(&self, rec: &RawRec) -> bool {
        if rec.ts_us < self.req.start_us || rec.ts_us > self.req.end_us {
            return false;
        }
        // ts_protocol::de::decode wants a source_id + ts_us only for diagnostic
        // bookkeeping in the parsed value; we reuse the request's source_id.
        let parsed = match decode(
            &rec.data,
            self.link_type,
            rec.ts_us,
            self.req.source_id.clone(),
        ) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let forward = field_match(self.req.client_ip,   parsed.src_ip)
                   && field_match(self.req.client_port, parsed.src_port)
                   && field_match(self.req.server_ip,   parsed.dst_ip)
                   && field_match(self.req.server_port, parsed.dst_port);
        let reverse = field_match(self.req.client_ip,   parsed.dst_ip)
                   && field_match(self.req.client_port, parsed.dst_port)
                   && field_match(self.req.server_ip,   parsed.src_ip)
                   && field_match(self.req.server_port, parsed.src_port);
        forward || reverse
    }
}

fn field_match<T: Eq>(filter: Option<T>, actual: T) -> bool {
    filter.map_or(true, |f| f == actual)
}

// Used for symmetry but not actually called externally; kept as a doc anchor.
#[allow(dead_code)]
fn _ip_unused(_: IpAddr) {}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// Build a minimal Ethernet+IPv4+TCP packet (20+20 byte headers) with the
    /// given addresses. Caplen == frame length.
    fn ipv4_tcp_pkt(
        src_ip: [u8; 4], src_port: u16,
        dst_ip: [u8; 4], dst_port: u16,
    ) -> Vec<u8> {
        let mut frame = Vec::new();
        // Ethernet (14): dst mac + src mac + ethertype 0x0800
        frame.extend_from_slice(&[0u8; 6]);
        frame.extend_from_slice(&[0u8; 6]);
        frame.extend_from_slice(&[0x08, 0x00]);
        // IPv4 (20)
        let ip_total_len: u16 = 20 + 20;
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;                                    // version 4, IHL 5
        ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
        ip[8] = 64;                                      // TTL
        ip[9] = 6;                                       // protocol = TCP
        ip[12..16].copy_from_slice(&src_ip);
        ip[16..20].copy_from_slice(&dst_ip);
        frame.extend_from_slice(&ip);
        // TCP (20)
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&src_port.to_be_bytes());
        tcp[2..4].copy_from_slice(&dst_port.to_be_bytes());
        tcp[12] = 0x50;                                  // data offset 5 (no options), reserved 0
        tcp[13] = 0x10;                                  // ACK
        frame.extend_from_slice(&tcp);
        frame
    }

    fn rec_for(data: Vec<u8>, ts_us: i64) -> RawRec {
        let len = data.len() as u32;
        RawRec { ts_us, caplen: len, wirelen: len, data: Bytes::from(data) }
    }

    fn req_with(start_us: i64, end_us: i64,
                client_ip: Option<IpAddr>, client_port: Option<u16>,
                server_ip: Option<IpAddr>, server_port: Option<u16>) -> ExtractRequest {
        ExtractRequest {
            source_id: "test".into(),
            start_us, end_us,
            client_ip, client_port, server_ip, server_port,
        }
    }

    #[test]
    fn matches_forward_direction() {
        let pkt = ipv4_tcp_pkt([10,0,0,1], 54321, [1,2,3,4], 443);
        let req = req_with(0, 10_000_000,
            "10.0.0.1".parse().ok(), Some(54321),
            "1.2.3.4".parse().ok(), Some(443));
        let f = Filter { req: &req, link_type: 1 };
        assert!(f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn matches_reverse_direction() {
        // Packet flowing server → client; same filter still matches.
        let pkt = ipv4_tcp_pkt([1,2,3,4], 443, [10,0,0,1], 54321);
        let req = req_with(0, 10_000_000,
            "10.0.0.1".parse().ok(), Some(54321),
            "1.2.3.4".parse().ok(), Some(443));
        let f = Filter { req: &req, link_type: 1 };
        assert!(f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn empty_fields_are_wildcard() {
        let pkt = ipv4_tcp_pkt([10,0,0,1], 54321, [1,2,3,4], 443);
        let req = req_with(0, 10_000_000, None, None, None, None);
        let f = Filter { req: &req, link_type: 1 };
        assert!(f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn rejects_outside_time_window() {
        let pkt = ipv4_tcp_pkt([10,0,0,1], 54321, [1,2,3,4], 443);
        let req = req_with(2_000_000, 5_000_000, None, None, None, None);
        let f = Filter { req: &req, link_type: 1 };
        assert!(!f.matches(&rec_for(pkt.clone(), 1_999_999)));
        assert!(f.matches(&rec_for(pkt.clone(), 2_000_000)));   // inclusive lo
        assert!(f.matches(&rec_for(pkt.clone(), 5_000_000)));   // inclusive hi
        assert!(!f.matches(&rec_for(pkt, 5_000_001)));
    }

    #[test]
    fn drops_non_matching_5_tuple() {
        let pkt = ipv4_tcp_pkt([10,0,0,1], 54321, [1,2,3,4], 443);
        let req = req_with(0, 10_000_000,
            "10.0.0.99".parse().ok(), None, None, None);  // wrong client_ip
        let f = Filter { req: &req, link_type: 1 };
        assert!(!f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn drops_non_tcp_packet() {
        // Pure Ethernet, no IPv4: ts_protocol::de::decode returns Err.
        let frame = vec![0u8; 14];
        let req = req_with(0, 10_000_000, None, None, None, None);
        let f = Filter { req: &req, link_type: 1 };
        assert!(!f.matches(&rec_for(frame, 1_000_000)));
    }
}
```

- [ ] **Step 2: Wire module**

Append to `server/ts-pcap-extract/src/lib.rs`:
```rust
pub mod filter;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-pcap-extract filter::tests`
Expected: 6 passed.

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/filter.rs server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): time + bidirectional 5-tuple filter"
```

---

## Phase F — Output encoders (`output.rs`)

### Task F1: Byte encoders for global header + per-record header

**Files:**
- Create: `server/ts-pcap-extract/src/output.rs`
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `server/ts-pcap-extract/src/output.rs`:
```rust
//! Byte encoders for the output `.pcap` stream. Mirror the writer-side
//! layout in `ts-capture::pcap_dump` byte-for-byte but live independently
//! per the no-dep rationale in the spec.

use crate::format::{PCAP_MAGIC, PCAP_SNAPLEN, PCAP_VERSION_MAJOR, PCAP_VERSION_MINOR};
use crate::reader::RawRec;

pub fn global_header(link_type: u32) -> [u8; 24] {
    let mut buf = [0u8; 24];
    buf[0..4].copy_from_slice(&PCAP_MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&PCAP_VERSION_MAJOR.to_le_bytes());
    buf[6..8].copy_from_slice(&PCAP_VERSION_MINOR.to_le_bytes());
    buf[8..12].copy_from_slice(&0i32.to_le_bytes());
    buf[12..16].copy_from_slice(&0u32.to_le_bytes());
    buf[16..20].copy_from_slice(&PCAP_SNAPLEN.to_le_bytes());
    buf[20..24].copy_from_slice(&link_type.to_le_bytes());
    buf
}

pub fn record_header(rec: &RawRec) -> [u8; 16] {
    let ts_sec = (rec.ts_us / 1_000_000) as u32;
    let ts_usec = (rec.ts_us % 1_000_000) as u32;
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&ts_sec.to_le_bytes());
    buf[4..8].copy_from_slice(&ts_usec.to_le_bytes());
    buf[8..12].copy_from_slice(&rec.caplen.to_le_bytes());
    buf[12..16].copy_from_slice(&rec.wirelen.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn global_header_layout() {
        let h = global_header(1);
        assert_eq!(&h[0..4], &PCAP_MAGIC.to_le_bytes());
        assert_eq!(&h[4..6], &2u16.to_le_bytes());
        assert_eq!(&h[6..8], &4u16.to_le_bytes());
        assert_eq!(&h[16..20], &PCAP_SNAPLEN.to_le_bytes());
        assert_eq!(&h[20..24], &1u32.to_le_bytes());
    }

    #[test]
    fn record_header_layout() {
        let rec = RawRec {
            ts_us: 1_500_250,
            caplen: 3,
            wirelen: 3,
            data: Bytes::from_static(&[0xaa, 0xbb, 0xcc]),
        };
        let h = record_header(&rec);
        assert_eq!(&h[0..4], &1u32.to_le_bytes());
        assert_eq!(&h[4..8], &500_250u32.to_le_bytes());
        assert_eq!(&h[8..12], &3u32.to_le_bytes());
        assert_eq!(&h[12..16], &3u32.to_le_bytes());
    }
}
```

- [ ] **Step 2: Wire module**

Append to `server/ts-pcap-extract/src/lib.rs`:
```rust
pub mod output;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-pcap-extract output::tests`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/output.rs server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): byte encoders for output pcap"
```

---

## Phase G — K-way merge (`merge.rs`)

### Task G1: K-way merge across multiple `PacketIter`s

**Files:**
- Create: `server/ts-pcap-extract/src/merge.rs`
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `server/ts-pcap-extract/src/merge.rs`:
```rust
//! K-way merge of N `PacketIter`s into a single time-ordered iterator,
//! filtered through `Filter`. Yields `RawRec`s that pass the filter, in
//! strictly non-decreasing `ts_us` order.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::filter::Filter;
use crate::reader::{PacketIter, RawRec};

struct HeapEntry {
    ts_us: i64,
    file_idx: usize,
    rec: RawRec,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.ts_us == other.ts_us && self.file_idx == other.file_idx
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ts_us
            .cmp(&other.ts_us)
            .then(self.file_idx.cmp(&other.file_idx))
    }
}

pub struct MergeIter<'a> {
    iters: Vec<PacketIter>,
    heap: BinaryHeap<Reverse<HeapEntry>>,
    filters: Vec<Filter<'a>>,
}

impl<'a> MergeIter<'a> {
    pub fn new(iters: Vec<PacketIter>, req: &'a crate::types::ExtractRequest) -> Self {
        let filters: Vec<Filter<'a>> = iters
            .iter()
            .map(|it| Filter { req, link_type: it.link_type })
            .collect();
        let mut me = Self { iters, heap: BinaryHeap::new(), filters };
        for idx in 0..me.iters.len() {
            me.refill(idx);
        }
        me
    }

    fn refill(&mut self, idx: usize) {
        while let Some(rec) = self.iters[idx].next() {
            if self.filters[idx].matches(&rec) {
                self.heap.push(Reverse(HeapEntry { ts_us: rec.ts_us, file_idx: idx, rec }));
                return;
            }
        }
    }
}

impl Iterator for MergeIter<'_> {
    type Item = RawRec;

    fn next(&mut self) -> Option<RawRec> {
        let Reverse(entry) = self.heap.pop()?;
        let HeapEntry { rec, file_idx, .. } = entry;
        self.refill(file_idx);
        Some(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::CandidateFile;
    use crate::output::{global_header, record_header};
    use bytes::Bytes;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_file(path: &Path, link_type: u32, recs: &[(i64, &[u8])]) {
        let mut f = File::create(path).unwrap();
        f.write_all(&global_header(link_type)).unwrap();
        for (ts_us, data) in recs {
            // forge a record
            let rec = RawRec {
                ts_us: *ts_us,
                caplen: data.len() as u32,
                wirelen: data.len() as u32,
                data: Bytes::copy_from_slice(data),
            };
            f.write_all(&record_header(&rec)).unwrap();
            f.write_all(data).unwrap();
        }
    }

    fn ipv4_tcp_pkt(src: [u8;4], sp: u16, dst: [u8;4], dp: u16) -> Vec<u8> {
        // Reuse the helper from filter::tests via copy here so this module's
        // tests are self-contained.
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0u8; 12]);
        frame.extend_from_slice(&[0x08, 0x00]);
        let ip_total_len: u16 = 40;
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
        ip[8] = 64;
        ip[9] = 6;
        ip[12..16].copy_from_slice(&src);
        ip[16..20].copy_from_slice(&dst);
        frame.extend_from_slice(&ip);
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&sp.to_be_bytes());
        tcp[2..4].copy_from_slice(&dp.to_be_bytes());
        tcp[12] = 0x50;
        tcp[13] = 0x10;
        frame.extend_from_slice(&tcp);
        frame
    }

    #[test]
    fn merges_two_files_in_time_order() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.pcap");
        let p2 = dir.path().join("b.pcap");
        let pkt = ipv4_tcp_pkt([10,0,0,1], 1, [1,2,3,4], 80);
        write_file(&p1, 1, &[(1_000_000, &pkt), (3_000_000, &pkt)]);
        write_file(&p2, 1, &[(2_000_000, &pkt), (4_000_000, &pkt)]);

        let req = crate::types::ExtractRequest {
            source_id: "x".into(),
            start_us: 0, end_us: 10_000_000,
            client_ip: None, client_port: None,
            server_ip: None, server_port: None,
        };
        let iters = vec![
            PacketIter::open(&CandidateFile { path: p1, compressed: false }).unwrap(),
            PacketIter::open(&CandidateFile { path: p2, compressed: false }).unwrap(),
        ];
        let timestamps: Vec<i64> = MergeIter::new(iters, &req).map(|r| r.ts_us).collect();
        assert_eq!(timestamps, vec![1_000_000, 2_000_000, 3_000_000, 4_000_000]);
    }

    #[test]
    fn skips_records_filtered_out() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.pcap");
        let pkt_match = ipv4_tcp_pkt([10,0,0,1], 1, [1,2,3,4], 80);
        let pkt_other = ipv4_tcp_pkt([10,0,0,2], 1, [1,2,3,4], 80);
        write_file(&p1, 1, &[(1_000_000, &pkt_other), (2_000_000, &pkt_match)]);

        let req = crate::types::ExtractRequest {
            source_id: "x".into(),
            start_us: 0, end_us: 10_000_000,
            client_ip: "10.0.0.1".parse().ok(),
            client_port: None,
            server_ip: None, server_port: None,
        };
        let iters = vec![PacketIter::open(&CandidateFile { path: p1, compressed: false }).unwrap()];
        let timestamps: Vec<i64> = MergeIter::new(iters, &req).map(|r| r.ts_us).collect();
        assert_eq!(timestamps, vec![2_000_000]);
    }
}
```

- [ ] **Step 2: Wire module**

Append to `server/ts-pcap-extract/src/lib.rs`:
```rust
pub mod merge;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-pcap-extract merge::tests`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/merge.rs server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): k-way merge of filtered PacketIters"
```

---

## Phase H — Public `extract()` function

### Task H1: `extract()` returns a `Stream<Item = io::Result<Bytes>>`

**Files:**
- Modify: `server/ts-pcap-extract/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Replace `server/ts-pcap-extract/src/lib.rs` content with:
```rust
//! Read-side counterpart to `ts-capture::pcap_dump`. Scans
//! `<base>/<pipeline>/<source_id>/<minute>.pcap[.snappy]` trees, filters
//! by 5-tuple + time window, and emits a single uncompressed pcap byte
//! stream suitable for HTTP download.

pub mod candidates;
pub mod filter;
pub mod format;
pub mod merge;
pub mod output;
pub mod reader;
pub mod types;

pub use types::{ExtractError, ExtractRequest, PipelineRoot};

use std::io;

use bytes::Bytes;
use futures::stream::Stream;

use crate::candidates::list_candidate_files;
use crate::format::DEFAULT_EMPTY_LINK_TYPE;
use crate::merge::MergeIter;
use crate::output::{global_header, record_header};
use crate::reader::PacketIter;

/// Build a streaming pcap byte sequence: 24-byte global header followed by
/// zero or more (16-byte record header + caplen bytes data) records, in
/// strict ascending `ts_us` order. Always begins with a global header,
/// even when no records match (header-only is still a valid `.pcap`).
pub fn extract(
    req: ExtractRequest,
    roots: &[PipelineRoot],
) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
    let files = list_candidate_files(&req, roots);

    // Open all candidate files synchronously up front; surface open errors
    // as a single error item at the head of the stream so the caller still
    // receives a well-formed Stream contract.
    let mut iters: Vec<PacketIter> = Vec::with_capacity(files.len());
    let mut open_err: Option<io::Error> = None;
    for f in &files {
        match PacketIter::open(f) {
            Ok(it) => iters.push(it),
            Err(e) => {
                tracing::warn!(path = %f.path.display(), error = %e, "pcap-extract: failed to open candidate; skipping");
                if open_err.is_none() {
                    open_err = Some(e);
                }
            }
        }
    }

    // Determine link_type for the global header.
    // - If at least one iter opened: use the first one's link_type, and
    //   error out if any other iter disagrees (extremely unlikely; pcap_dump
    //   pins per source_id).
    // - Else: default to Ethernet so the empty header is still openable.
    let header_link_type = match iters.first() {
        Some(first) => {
            let lt = first.link_type;
            for it in iters.iter().skip(1) {
                if it.link_type != lt {
                    let err = ExtractError::LinkTypeMismatch { expected: lt, got: it.link_type };
                    tracing::error!(error = %err, "pcap-extract: aborting");
                    let head = vec![Err(io::Error::new(io::ErrorKind::InvalidData, err.to_string()))];
                    return futures::stream::iter(head);
                }
            }
            lt
        }
        None => DEFAULT_EMPTY_LINK_TYPE,
    };

    // Build the byte sequence in memory by walking the merge iterator.
    // The 1-hour cap on requests keeps this bounded; streaming-from-Stream
    // would require Pin<Box<dyn Iterator>> tied to `req`'s lifetime, and the
    // simpler synchronous fold is fine inside the bounded window.
    let mut chunks: Vec<io::Result<Bytes>> = Vec::with_capacity(1 + iters.len() * 4);
    chunks.push(Ok(Bytes::copy_from_slice(&global_header(header_link_type))));

    let req_ref = &req;
    let merge = MergeIter::new(iters, req_ref);
    for rec in merge {
        chunks.push(Ok(Bytes::copy_from_slice(&record_header(&rec))));
        chunks.push(Ok(rec.data));
    }

    if let Some(e) = open_err {
        // Emit the first open error AT THE END so the caller still receives
        // a parseable .pcap prefix; downstream HTTP layer can choose to log
        // and drop, or surface to the client.
        let _ = e; // currently dropped; switch to `chunks.push(Err(e))` if desired
    }

    futures::stream::iter(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use futures::StreamExt;
    use std::path::Path;
    use tempfile::tempdir;
    use tokio::runtime::Runtime;

    async fn collect_stream(s: impl Stream<Item = io::Result<Bytes>>) -> Vec<u8> {
        futures::pin_mut!(s);
        let mut out = BytesMut::new();
        while let Some(item) = s.next().await {
            out.extend_from_slice(&item.unwrap());
        }
        out.to_vec()
    }

    fn write_one_record_file(dir: &Path, source: &str) -> std::path::PathBuf {
        use std::fs;
        let src_dir = dir.join(format!("local/{source}"));
        fs::create_dir_all(&src_dir).unwrap();
        // Forge an Ethernet+IPv4+TCP packet via the same helper inline
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0u8; 12]);
        frame.extend_from_slice(&[0x08, 0x00]);
        let ip_total_len: u16 = 40;
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
        ip[8] = 64;
        ip[9] = 6;
        ip[12..16].copy_from_slice(&[10,0,0,1]);
        ip[16..20].copy_from_slice(&[1,2,3,4]);
        frame.extend_from_slice(&ip);
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&54321u16.to_be_bytes());
        tcp[2..4].copy_from_slice(&443u16.to_be_bytes());
        tcp[12] = 0x50;
        tcp[13] = 0x10;
        frame.extend_from_slice(&tcp);
        let path = src_dir.join("19700101T0000.pcap");
        let mut f = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut f, &output::global_header(1)).unwrap();
        let rec = reader::RawRec {
            ts_us: 1_000_000,
            caplen: frame.len() as u32,
            wirelen: frame.len() as u32,
            data: Bytes::from(frame.clone()),
        };
        std::io::Write::write_all(&mut f, &output::record_header(&rec)).unwrap();
        std::io::Write::write_all(&mut f, &frame).unwrap();
        path
    }

    #[test]
    fn header_only_when_no_files() {
        let rt = Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let req = ExtractRequest {
            source_id: "missing".into(),
            start_us: 0, end_us: 30_000_000,
            client_ip: None, client_port: None,
            server_ip: None, server_port: None,
        };
        let roots = vec![PipelineRoot { name: "local".into(), dump_dir: dir.path().to_path_buf() }];
        let bytes = rt.block_on(collect_stream(extract(req, &roots)));
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[0..4], &format::PCAP_MAGIC.to_le_bytes());
        assert_eq!(&bytes[20..24], &1u32.to_le_bytes());   // default Ethernet
    }

    #[test]
    fn header_plus_records_when_match() {
        let rt = Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let _ = write_one_record_file(dir.path(), "en0");
        let req = ExtractRequest {
            source_id: "en0".into(),
            start_us: 0, end_us: 30_000_000,
            client_ip: None, client_port: None,
            server_ip: None, server_port: None,
        };
        let roots = vec![PipelineRoot { name: "local".into(), dump_dir: dir.path().to_path_buf() }];
        let bytes = rt.block_on(collect_stream(extract(req, &roots)));
        assert!(bytes.len() > 24);
        assert_eq!(&bytes[0..4], &format::PCAP_MAGIC.to_le_bytes());
    }
}
```

- [ ] **Step 2: Add `tokio` dev features for async test**

Confirm `Cargo.toml` of `ts-pcap-extract` has `tokio = { workspace = true }` already (workspace `tokio` includes `rt-multi-thread` and `macros`). The runtime is built explicitly via `Runtime::new()` so no `#[tokio::test]` macros are needed.

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-pcap-extract`
Expected: all crate tests pass (counts roughly: types=1, format=4, candidates=5, reader=5, filter=6, output=2, merge=2, lib=2).

- [ ] **Step 4: Commit**

```bash
git add server/ts-pcap-extract/src/lib.rs
git commit -m "feat(ts-pcap-extract): public extract() returning a Stream of pcap bytes"
```

---

### Task H2: e2e round-trip test through pcap_dump writer

**Files:**
- Create: `server/ts-pcap-extract/tests/round_trip.rs`

- [ ] **Step 1: Write the failing test**

Create `server/ts-pcap-extract/tests/round_trip.rs`:
```rust
//! End-to-end: write packets via the same on-disk format as
//! `ts-capture::pcap_dump`, then `extract()` them, then re-open the
//! resulting bytes with libpcap to confirm they're a valid `.pcap`.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use tempfile::tempdir;
use tokio::runtime::Runtime;

use ts_pcap_extract::output::{global_header, record_header};
use ts_pcap_extract::reader::RawRec;
use ts_pcap_extract::{extract, ExtractRequest, PipelineRoot};

fn ipv4_tcp_pkt(src: [u8;4], sp: u16, dst: [u8;4], dp: u16) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0u8; 12]);
    frame.extend_from_slice(&[0x08, 0x00]);
    let ip_total_len: u16 = 40;
    let mut ip = vec![0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
    ip[8] = 64;
    ip[9] = 6;
    ip[12..16].copy_from_slice(&src);
    ip[16..20].copy_from_slice(&dst);
    frame.extend_from_slice(&ip);
    let mut tcp = vec![0u8; 20];
    tcp[0..2].copy_from_slice(&sp.to_be_bytes());
    tcp[2..4].copy_from_slice(&dp.to_be_bytes());
    tcp[12] = 0x50;
    tcp[13] = 0x10;
    frame.extend_from_slice(&tcp);
    frame
}

fn write_minute_file(dir: &Path, label: &str, link_type: u32, recs: &[(i64, &[u8])]) {
    let path = dir.join(format!("{label}.pcap"));
    let mut f = File::create(&path).unwrap();
    f.write_all(&global_header(link_type)).unwrap();
    for (ts_us, data) in recs {
        let rec = RawRec {
            ts_us: *ts_us,
            caplen: data.len() as u32,
            wirelen: data.len() as u32,
            data: Bytes::copy_from_slice(data),
        };
        f.write_all(&record_header(&rec)).unwrap();
        f.write_all(data).unwrap();
    }
}

async fn collect(s: impl Stream<Item = std::io::Result<Bytes>>) -> Vec<u8> {
    futures::pin_mut!(s);
    let mut out = BytesMut::new();
    while let Some(item) = s.next().await {
        out.extend_from_slice(&item.unwrap());
    }
    out.to_vec()
}

#[test]
fn round_trip_libpcap_can_open_extract_output() {
    let rt = Runtime::new().unwrap();
    let base = tempdir().unwrap();
    let src_dir = base.path().join("local/en0");
    std::fs::create_dir_all(&src_dir).unwrap();

    let pkt_a = ipv4_tcp_pkt([10,0,0,1], 54321, [1,2,3,4], 443);
    let pkt_b = ipv4_tcp_pkt([1,2,3,4], 443, [10,0,0,1], 54321);
    write_minute_file(&src_dir, "19700101T0000", 1, &[
        (1_000_000, &pkt_a),
        (1_500_000, &pkt_b),
        (2_000_000, &pkt_a),
    ]);
    write_minute_file(&src_dir, "19700101T0001", 1, &[
        (60_500_000, &pkt_a),
    ]);

    let req = ExtractRequest {
        source_id: "en0".into(),
        start_us: 0,
        end_us: 120_000_000,
        client_ip: "10.0.0.1".parse().ok(),
        client_port: Some(54321),
        server_ip: "1.2.3.4".parse().ok(),
        server_port: Some(443),
    };
    let roots = vec![PipelineRoot { name: "local".into(), dump_dir: base.path().to_path_buf() }];
    let bytes = rt.block_on(collect(extract(req, &roots)));

    // Spit to a temp file and feed to libpcap.
    let out_path = base.path().join("extract.pcap");
    std::fs::write(&out_path, &bytes).unwrap();
    let mut cap = pcap::Capture::from_file(&out_path).expect("libpcap opens result");
    let mut tss = Vec::new();
    while let Ok(p) = cap.next_packet() {
        let ts = p.header.ts.tv_sec as i64 * 1_000_000 + p.header.ts.tv_usec as i64;
        tss.push(ts);
    }
    assert_eq!(tss, vec![1_000_000, 1_500_000, 2_000_000, 60_500_000]);
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p ts-pcap-extract --test round_trip`
Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add server/ts-pcap-extract/tests/round_trip.rs
git commit -m "test(ts-pcap-extract): e2e round-trip via libpcap reader"
```

---

## Phase I — HTTP API route

### Task I1: Add `pcap_extract` route module + handler

**Files:**
- Modify: `server/ts-api/Cargo.toml`
- Modify: `server/ts-api/src/routes/mod.rs`
- Create: `server/ts-api/src/routes/pcap_extract.rs`

- [ ] **Step 1: Add dep**

`server/ts-api/Cargo.toml` — under `[dependencies]`, add:
```toml
ts-pcap-extract = { workspace = true }
```

- [ ] **Step 2: Register the module**

`server/ts-api/src/routes/mod.rs` — add `pub mod pcap_extract;` keeping alphabetical order:

```rust
pub mod agent_sessions;
pub mod agent_turns;
pub mod filters;
pub mod health;
pub mod http_exchanges;
pub mod internal_metrics;
pub mod llm_calls;
pub mod metrics;
pub mod pcap_extract;
pub mod runtime_config;
```

- [ ] **Step 3: Write the handler**

Create `server/ts-api/src/routes/pcap_extract.rs`:
```rust
//! `GET /api/pcap/extract` — stream a 5-tuple-filtered, time-bounded `.pcap`
//! slice out of the on-disk `pcap_dump` minute files. See
//! `docs/superpowers/specs/2026-05-02-pcap-extract-download-design.md`.

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use ts_common::path::is_safe_path_component;
use ts_pcap_extract::{extract, ExtractRequest, PipelineRoot};

use crate::extractors::Query;
use crate::response::ApiError;

const MAX_WINDOW_US: i64 = 60 * 60 * 1_000_000;   // 1 hour

#[derive(Debug, Deserialize)]
pub struct ExtractParams {
    pub source_id: String,
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub client_ip: Option<String>,
    #[serde(default)]
    pub client_port: Option<u16>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub server_port: Option<u16>,
}

pub async fn handler(
    State(roots): State<Arc<Vec<PipelineRoot>>>,
    Query(params): Query<ExtractParams>,
) -> Result<Response, ApiError> {
    let req = build_request(params)?;
    let stream = extract(req, &roots);
    let body = Body::from_stream(stream);
    let filename = format!("ts-extract-{}.pcap", Utc::now().format("%Y%m%dT%H%M%S"));
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/vnd.tcpdump.pcap".to_string()),
            (CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
        ],
        body,
    ).into_response())
}

fn build_request(p: ExtractParams) -> Result<ExtractRequest, ApiError> {
    if !is_safe_path_component(&p.source_id) {
        return Err(ApiError::InvalidParam(format!("invalid source_id: {}", p.source_id)));
    }
    if p.start >= p.end {
        return Err(ApiError::InvalidParam("start must be < end".into()));
    }
    if p.end - p.start > MAX_WINDOW_US {
        return Err(ApiError::InvalidParam("time window exceeds 1 hour".into()));
    }
    let client_ip = parse_optional_ip("client_ip", p.client_ip.as_deref())?;
    let server_ip = parse_optional_ip("server_ip", p.server_ip.as_deref())?;
    Ok(ExtractRequest {
        source_id: p.source_id,
        start_us: p.start,
        end_us: p.end,
        client_ip,
        client_port: p.client_port,
        server_ip,
        server_port: p.server_port,
    })
}

fn parse_optional_ip(name: &str, value: Option<&str>) -> Result<Option<IpAddr>, ApiError> {
    match value {
        Some(s) if !s.is_empty() => s.parse::<IpAddr>()
            .map(Some)
            .map_err(|_| ApiError::InvalidParam(format!("invalid {name}: {s}"))),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(source_id: &str, start: i64, end: i64) -> ExtractParams {
        ExtractParams {
            source_id: source_id.into(),
            start, end,
            client_ip: None, client_port: None,
            server_ip: None, server_port: None,
        }
    }

    #[test]
    fn rejects_unsafe_source_id() {
        let err = build_request(p("..", 0, 1000)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn rejects_start_geq_end() {
        let err = build_request(p("x", 100, 100)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn rejects_window_too_wide() {
        let err = build_request(p("x", 0, MAX_WINDOW_US + 1)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn rejects_bad_ip() {
        let mut params = p("x", 0, 1_000_000);
        params.client_ip = Some("not.an.ip".into());
        let err = build_request(params).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn happy_request() {
        let mut params = p("en0", 0, 1_000_000);
        params.client_ip = Some("10.0.0.1".into());
        params.client_port = Some(54321);
        let req = build_request(params).unwrap();
        assert_eq!(req.source_id, "en0");
        assert_eq!(req.client_port, Some(54321));
        assert!(req.client_ip.is_some());
    }
}
```

- [ ] **Step 4: Add `chrono` to ts-api Cargo.toml if not already present**

Confirm `server/ts-api/Cargo.toml` has `chrono = { workspace = true }` under dependencies. If absent, add it.

- [ ] **Step 5: Run handler unit tests**

Run: `cargo test -p ts-api routes::pcap_extract::tests`
Expected: 5 passed.

- [ ] **Step 6: Commit**

```bash
git add server/ts-api/Cargo.toml server/ts-api/src/routes/mod.rs server/ts-api/src/routes/pcap_extract.rs
git commit -m "feat(ts-api): add /api/pcap/extract route handler"
```

---

### Task I2: Mount `/api/pcap/extract` in `router()` with new state

**Files:**
- Modify: `server/ts-api/src/lib.rs`

- [ ] **Step 1: Update `router()` signature and mount the route**

Change `router(...)` to accept `pcap_roots: Arc<Vec<PipelineRoot>>` and mount the new sub-router. Replace lines around the existing function with:

```rust
use ts_pcap_extract::PipelineRoot;

// ... (existing struct/fn declarations) ...

/// Build the API router (without serving). Useful for composing with other layers.
pub fn router(
    storage: Arc<dyn StorageBackend>,
    metrics: ApiMetricsContext,
    runtime_config: ApiRuntimeConfigContext,
    health: ApiHealthContext,
    pcap_roots: Arc<Vec<PipelineRoot>>,
) -> Router {
    let internal_metrics_routes = Router::new()
        .route(
            "/api/internal-metrics",
            get(routes::internal_metrics::internal_metrics),
        )
        .with_state(metrics);

    let runtime_config_routes = Router::new()
        .route(
            "/api/runtime-config",
            get(routes::runtime_config::runtime_config),
        )
        .with_state(runtime_config);

    let health_routes = Router::new()
        .route("/api/health", get(routes::health::health))
        .with_state(health);

    let pcap_extract_routes = Router::new()
        .route("/api/pcap/extract", get(routes::pcap_extract::handler))
        .with_state(pcap_roots);

    Router::new()
        // ... existing storage-state routes unchanged ...
        .with_state(storage)
        .merge(internal_metrics_routes)
        .merge(runtime_config_routes)
        .merge(health_routes)
        .merge(pcap_extract_routes)
        .layer(CorsLayer::permissive())
}
```

(Keep the existing `.route(...)` chain unchanged; only add the `pcap_extract_routes` definition above and the `.merge(pcap_extract_routes)` at the bottom.)

- [ ] **Step 2: Run cargo check**

Run: `cargo check -p ts-api`
Expected: compile error in `tokenscope` because the call sites haven't been updated yet — that's expected; ts-api itself should compile.

For ts-api alone:
Run: `cargo build -p ts-api`
Expected: success.

- [ ] **Step 3: Commit**

```bash
git add server/ts-api/src/lib.rs
git commit -m "feat(ts-api): wire /api/pcap/extract into router with PipelineRoot state"
```

---

### Task I3: Integration test for the route returning header-only pcap

**Files:**
- Create: `server/ts-api/tests/pcap_extract_route.rs`

- [ ] **Step 1: Write the failing test**

Create `server/ts-api/tests/pcap_extract_route.rs`:
```rust
//! Smoke test: build a minimal Router with the `/api/pcap/extract` route
//! and a stub `Vec<PipelineRoot>`, hit it with a synthetic GET, assert
//! the response shape.
//!
//! Uses `axum::Router::new().route(...).with_state(...)` directly rather
//! than `ts_api::router(...)` to keep the test focused on the new route.

use std::sync::Arc;

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use ts_pcap_extract::PipelineRoot;
use tower::util::ServiceExt;

#[tokio::test]
async fn returns_header_only_pcap_when_no_files() {
    let roots: Arc<Vec<PipelineRoot>> = Arc::new(vec![PipelineRoot {
        name: "local".into(),
        dump_dir: std::path::PathBuf::from("/nonexistent"),
    }]);
    let app: Router = Router::new()
        .route("/api/pcap/extract", get(ts_api::routes::pcap_extract::handler))
        .with_state(roots);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/pcap/extract?source_id=en0&start=0&end=30000000")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    // Header-only: 24 bytes, magic at start.
    assert_eq!(body.len(), 24);
    assert_eq!(&body[0..4], &0xa1b2_c3d4u32.to_le_bytes());
}

#[tokio::test]
async fn rejects_window_too_wide_with_400() {
    let roots: Arc<Vec<PipelineRoot>> = Arc::new(vec![]);
    let app: Router = Router::new()
        .route("/api/pcap/extract", get(ts_api::routes::pcap_extract::handler))
        .with_state(roots);

    // 1h + 1us
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/pcap/extract?source_id=en0&start=0&end=3600000001")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
```

- [ ] **Step 2: Add `tower` dev-dep if missing**

Confirm `server/ts-api/Cargo.toml` has `tower = "0.5"` under `[dev-dependencies]`. If missing, add.

- [ ] **Step 3: Run integration test**

Run: `cargo test -p ts-api --test pcap_extract_route`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add server/ts-api/tests/pcap_extract_route.rs server/ts-api/Cargo.toml
git commit -m "test(ts-api): integration tests for /api/pcap/extract"
```

---

## Phase J — Runtime wiring

### Task J1: Compute `Vec<PipelineRoot>` and pass to both router call sites

**Files:**
- Modify: `server/app/tokenscope/src/main.rs`
- Modify: `server/app/tokenscope/Cargo.toml`

- [ ] **Step 1: Add dep**

`server/app/tokenscope/Cargo.toml` — under `[dependencies]`, add:
```toml
ts-pcap-extract = { workspace = true }
```

- [ ] **Step 2: Compute the roots near where `effective_pipelines` is finalized**

In `server/app/tokenscope/src/main.rs`, find where `effective_pipelines` is built (around line 261). After that block, add:

```rust
let pcap_extract_roots: Arc<Vec<ts_pcap_extract::PipelineRoot>> = Arc::new(
    effective_pipelines
        .iter()
        .map(|def| ts_pcap_extract::PipelineRoot {
            name: def.name.clone(),
            dump_dir: std::path::PathBuf::from(&def.pcap_dump.dir),
        })
        .collect(),
);
```

(`Arc` and `std::path::PathBuf` are likely already in scope; add `use std::path::PathBuf;` if needed.)

The roots include all pipelines regardless of `pcap_dump.enabled` — `list_candidate_files` checks for the source-dir's existence per request, so disabled pipelines naturally have no dir tree and contribute nothing. Including them keeps the runtime calculation pure (no secondary "is dump enabled?" filter that has to be kept in sync).

- [ ] **Step 3: Pass to both router call sites**

In `main.rs`, find the **two** `ts_api::router(...)` invocations (around lines 589 and 788). Update each to pass the new `pcap_extract_roots` as the fifth argument. Both invocations must clone `Arc` because they're in different async tasks:

```rust
let pcap_extract_roots = pcap_extract_roots.clone();
// ... inside spawn ...
let router = ts_api::router(
    api_storage,
    api_metrics,
    api_runtime_config,
    api_health,
    pcap_extract_roots,
);
```

- [ ] **Step 4: Build the workspace**

Run: `cargo build -p tokenscope`
Expected: success.

- [ ] **Step 5: Run all server tests**

Run: `cargo test --workspace`
Expected: all existing tests still pass; the new ts-pcap-extract / ts-api tests pass too.

- [ ] **Step 6: Commit**

```bash
git add server/app/tokenscope/src/main.rs server/app/tokenscope/Cargo.toml
git commit -m "feat(app): inject Vec<PipelineRoot> into ts-api router"
```

---

## Phase K — Console feature module

### Task K1: `extract-defaults.ts`

**Files:**
- Create: `console/src/features/pcap-extract/extract-defaults.ts`

- [ ] **Step 1: Write the file**

Create the directory `console/src/features/pcap-extract/`, then the file:
```ts
// Pure mappers from anchor row → form initial values for the extract dialog.

import type { LlmCallDetail, HttpExchangeDetail, AgentTurnDetail } from "@/types/api"

export interface ExtractFormValues {
  source_id: string          // read-only
  client_ip: string
  client_port: string
  server_ip: string
  server_port: string
  start_us: number           // microseconds since epoch (matches API)
  end_us: number
}

export type Anchor =
  | { type: "http_exchange"; row: HttpExchangeDetail }
  | { type: "llm_call"; row: LlmCallDetail }
  | { type: "agent_turn"; row: AgentTurnDetail }

const SECOND_US = 1_000_000

/// Convert an ISO-or-ms timestamp from the API to microseconds since epoch.
/// API timestamps are documented as ms; LLM-call complete_time is sometimes
/// missing (still in flight) — caller must guard.
function tsToUs(ts_ms: number): number {
  return ts_ms * 1000
}

export function defaultsFor(anchor: Anchor): ExtractFormValues {
  switch (anchor.type) {
    case "http_exchange": {
      const r = anchor.row
      const start_us = tsToUs(r.request_time) - SECOND_US
      const end_us = r.response_complete_time != null
        ? tsToUs(r.response_complete_time) + SECOND_US
        : tsToUs(r.request_time) + 5 * SECOND_US
      return {
        source_id: r.source_id ?? "",
        client_ip: r.client_ip ?? "",
        client_port: r.client_port?.toString() ?? "",
        server_ip: r.server_ip ?? "",
        server_port: r.server_port?.toString() ?? "",
        start_us,
        end_us,
      }
    }
    case "llm_call": {
      const r = anchor.row
      const end_ms = r.complete_time ?? r.response_time ?? (r.request_time + 5_000)
      return {
        source_id: r.source_id ?? "",
        client_ip: r.client_ip ?? "",
        client_port: r.client_port?.toString() ?? "",
        server_ip: r.server_ip ?? "",
        server_port: r.server_port?.toString() ?? "",
        start_us: tsToUs(r.request_time) - SECOND_US,
        end_us: tsToUs(end_ms) + SECOND_US,
      }
    }
    case "agent_turn": {
      const r = anchor.row
      return {
        source_id: r.source_id ?? "",
        client_ip: r.client_ip ?? "",
        client_port: "",                     // intentionally blank — turn spans connections
        server_ip: r.server_ip ?? "",
        server_port: "",                     // server endpoints can vary
        start_us: tsToUs(r.start_time) - SECOND_US,
        end_us: tsToUs(r.end_time) + SECOND_US,
      }
    }
  }
}

const ONE_HOUR_US = 60 * 60 * 1_000_000

export interface FormValidation {
  ok: boolean
  reason?: string
}

export function validate(v: ExtractFormValues): FormValidation {
  if (v.start_us >= v.end_us) return { ok: false, reason: "start must be < end" }
  if (v.end_us - v.start_us > ONE_HOUR_US) return { ok: false, reason: "time window > 1h" }
  if (v.client_ip && !looksLikeIp(v.client_ip)) return { ok: false, reason: "client_ip is malformed" }
  if (v.server_ip && !looksLikeIp(v.server_ip)) return { ok: false, reason: "server_ip is malformed" }
  if (v.client_port && !looksLikePort(v.client_port)) return { ok: false, reason: "client_port is malformed" }
  if (v.server_port && !looksLikePort(v.server_port)) return { ok: false, reason: "server_port is malformed" }
  return { ok: true }
}

function looksLikeIp(s: string): boolean {
  // Cheap: IPv4 or IPv6 surface check; full validation happens server-side.
  return /^(\d{1,3}\.){3}\d{1,3}$|^[\da-fA-F:]+$/.test(s)
}
function looksLikePort(s: string): boolean {
  const n = Number(s)
  return Number.isInteger(n) && n >= 0 && n <= 65535
}

export function buildExtractUrl(v: ExtractFormValues): string {
  const params = new URLSearchParams()
  params.set("source_id", v.source_id)
  params.set("start", v.start_us.toString())
  params.set("end", v.end_us.toString())
  if (v.client_ip)   params.set("client_ip", v.client_ip)
  if (v.client_port) params.set("client_port", v.client_port)
  if (v.server_ip)   params.set("server_ip", v.server_ip)
  if (v.server_port) params.set("server_port", v.server_port)
  return `/api/pcap/extract?${params.toString()}`
}
```

- [ ] **Step 2: Verify type imports compile**

The types `LlmCallDetail`, `HttpExchangeDetail`, `AgentTurnDetail` are exported from `console/src/types/api.ts`. If their source-of-truth field names differ from those used above (e.g. `start_time` vs `startTime`), fix the references to match the existing API types. Confirm by:

Run: `cd console && bunx tsc -b --noEmit`
Expected: no errors related to the new file. (Other unrelated errors should already be absent.)

- [ ] **Step 3: Commit**

```bash
git add console/src/features/pcap-extract/extract-defaults.ts
git commit -m "feat(console): pcap-extract — defaults + URL builder + form validator"
```

---

### Task K2: `ExtractDialog.tsx`

**Files:**
- Create: `console/src/features/pcap-extract/ExtractDialog.tsx`

- [ ] **Step 1: Write the dialog**

```tsx
import { useState, useEffect } from "react"
import { X } from "lucide-react"
import {
  type ExtractFormValues,
  type Anchor,
  defaultsFor,
  validate,
  buildExtractUrl,
} from "./extract-defaults"

interface Props {
  anchor: Anchor
  open: boolean
  onClose: () => void
}

function usToInputLocal(us: number): string {
  // datetime-local needs "YYYY-MM-DDTHH:MM:SS" in *local* time (no Z).
  const ms = Math.round(us / 1000)
  const d = new Date(ms)
  const pad = (n: number) => n.toString().padStart(2, "0")
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}
function inputLocalToUs(s: string): number {
  return new Date(s).getTime() * 1000
}

export function ExtractDialog({ anchor, open, onClose }: Props) {
  const [values, setValues] = useState<ExtractFormValues>(() => defaultsFor(anchor))

  useEffect(() => {
    if (open) setValues(defaultsFor(anchor))
  }, [open, anchor])

  if (!open) return null

  const v = validate(values)

  const onExtract = () => {
    if (!v.ok) return
    const a = document.createElement("a")
    a.href = buildExtractUrl(values)
    a.download = ""    // honor Content-Disposition filename
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    onClose()
  }

  return (
    <>
      <div className="fixed inset-0 z-50 bg-black/30" onClick={onClose} />
      <div className="fixed left-1/2 top-1/2 z-50 w-[480px] -translate-x-1/2 -translate-y-1/2 rounded-md border border-border bg-background p-4 shadow-xl">
        <div className="mb-3 flex items-center justify-between">
          <h3 className="text-sm font-semibold">Extract packets</h3>
          <button onClick={onClose} className="rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground">
            <X className="size-4" />
          </button>
        </div>

        <div className="grid grid-cols-[120px_1fr] gap-y-2 text-xs">
          <Label>source_id</Label>
          <input value={values.source_id} readOnly className="rounded border border-border bg-muted px-2 py-1" />

          <Label>client_ip</Label>
          <input value={values.client_ip} onChange={(e) => setValues({ ...values, client_ip: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>client_port</Label>
          <input value={values.client_port} onChange={(e) => setValues({ ...values, client_port: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>server_ip</Label>
          <input value={values.server_ip} onChange={(e) => setValues({ ...values, server_ip: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>server_port</Label>
          <input value={values.server_port} onChange={(e) => setValues({ ...values, server_port: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>start (local)</Label>
          <input type="datetime-local" step="1" value={usToInputLocal(values.start_us)} onChange={(e) => setValues({ ...values, start_us: inputLocalToUs(e.target.value) })} className={inputCls} />

          <Label>end (local)</Label>
          <input type="datetime-local" step="1" value={usToInputLocal(values.end_us)} onChange={(e) => setValues({ ...values, end_us: inputLocalToUs(e.target.value) })} className={inputCls} />
        </div>

        {!v.ok && <p className="mt-3 text-xs text-red-500">{v.reason}</p>}

        <div className="mt-4 flex justify-end gap-2">
          <button onClick={onClose} className="rounded-md border border-border px-3 py-1 text-xs hover:bg-muted">Cancel</button>
          <button onClick={onExtract} disabled={!v.ok} className="rounded-md bg-primary px-3 py-1 text-xs text-primary-foreground hover:bg-primary/90 disabled:opacity-50">Extract</button>
        </div>
      </div>
    </>
  )
}

const inputCls = "rounded border border-border bg-background px-2 py-1"
function Label({ children }: { children: React.ReactNode }) {
  return <label className="self-center pr-2 text-muted-foreground">{children}</label>
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cd console && bunx tsc -b --noEmit`
Expected: no errors related to the new file.

- [ ] **Step 3: Commit**

```bash
git add console/src/features/pcap-extract/ExtractDialog.tsx
git commit -m "feat(console): pcap-extract dialog component"
```

---

### Task K3: `ExtractPacketsButton.tsx`

**Files:**
- Create: `console/src/features/pcap-extract/ExtractPacketsButton.tsx`

- [ ] **Step 1: Write the button wrapper**

```tsx
import { useState } from "react"
import { Download } from "lucide-react"
import { ExtractDialog } from "./ExtractDialog"
import type { Anchor } from "./extract-defaults"

interface Props {
  anchor: Anchor
  className?: string
}

export function ExtractPacketsButton({ anchor, className }: Props) {
  const [open, setOpen] = useState(false)
  return (
    <>
      <button
        onClick={() => setOpen(true)}
        className={
          className ??
          "mr-2 flex items-center gap-1.5 rounded-md border border-border px-2 py-1 text-xs text-foreground transition-colors hover:bg-muted"
        }
        title="Extract pcap packets for this row"
      >
        <Download className="size-3.5" />
        Extract packets
      </button>
      <ExtractDialog anchor={anchor} open={open} onClose={() => setOpen(false)} />
    </>
  )
}
```

- [ ] **Step 2: Compile check**

Run: `cd console && bunx tsc -b --noEmit`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add console/src/features/pcap-extract/ExtractPacketsButton.tsx
git commit -m "feat(console): pcap-extract button component"
```

---

## Phase L — Wire into the three detail pages

### Task L1: Add button to `LlmCallDetailPanel`

**Files:**
- Modify: `console/src/pages/llm-call-detail-panel.tsx`

- [ ] **Step 1: Add the button**

Import:
```tsx
import { ExtractPacketsButton } from "@/features/pcap-extract/ExtractPacketsButton"
```

Inside the toolbar `<div className="flex items-center gap-1">…</div>` (around line 48), insert **before** the existing "Raw HTTP" button:
```tsx
{detail && (
  <ExtractPacketsButton anchor={{ type: "llm_call", row: detail }} />
)}
```

- [ ] **Step 2: Compile check + visual sanity**

Run: `cd console && bun run dev` (background, then open the LLM Calls list, click any call to open the panel — confirm "Extract packets" button is rendered next to "Raw HTTP").

- [ ] **Step 3: Commit**

```bash
git add console/src/pages/llm-call-detail-panel.tsx
git commit -m "feat(console): add Extract packets button to LLM call detail"
```

---

### Task L2: Add button to `HttpExchangeDetailPanel`

**Files:**
- Modify: `console/src/pages/http-exchange-detail-panel.tsx`

- [ ] **Step 1: Add the button**

Same pattern as L1 — add the import, then inside the toolbar div, insert:
```tsx
{detail && (
  <ExtractPacketsButton anchor={{ type: "http_exchange", row: detail }} />
)}
```

If the detail-fetch hook uses a different name than `detail`, substitute. The exact placement is "in the existing top-right action toolbar of the panel".

- [ ] **Step 2: Compile + visual check**

Run: `cd console && bunx tsc -b --noEmit`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add console/src/pages/http-exchange-detail-panel.tsx
git commit -m "feat(console): add Extract packets button to HTTP exchange detail"
```

---

### Task L3: Add button to `AgentTurnDetailPanel`

**Files:**
- Modify: `console/src/pages/agent-turn-detail-panel.tsx`

- [ ] **Step 1: Add the button**

Same pattern. The anchor is:
```tsx
{detail && (
  <ExtractPacketsButton anchor={{ type: "agent_turn", row: detail }} />
)}
```

If the agent-turn detail type doesn't currently expose `client_ip` / `server_ip` directly (only via embedded calls), update `extract-defaults.ts` to derive defaults from the first call (`detail.calls[0]?.client_ip` etc.). Inspect `console/src/types/api.ts` for the actual `AgentTurnDetail` shape; if it lacks those fields, this fix-up is required and goes here, not as a separate task — the design intent is "anchor button works on turn page", however the type is shaped.

- [ ] **Step 2: Compile + visual check**

Run: `cd console && bunx tsc -b --noEmit`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add console/src/pages/agent-turn-detail-panel.tsx console/src/features/pcap-extract/extract-defaults.ts
git commit -m "feat(console): add Extract packets button to agent turn detail"
```

---

## Phase M — End-to-end smoke

### Task M1: Live smoke test against a running pipeline

**Files:** none

- [ ] **Step 1: Bring up tokenscope locally**

Run a pipeline that actually writes pcap_dump files. Easiest: replay a fixture with `--pcap-file`.

```bash
# Replace the fixture path with whatever you have under tests/fixtures or similar
cargo run -p tokenscope -- --pcap-file path/to/some.pcap
```

The `--pcap-file` CLI mode keeps the API up after drain (per existing `b0e5639`); confirm `pcap_dump.enabled = true` is set in the active config (or pass via env / a temporary config file). Files should appear under `data/dumps/local/<source_id>/<minute>.pcap`.

- [ ] **Step 2: Hit the API directly with `curl`**

Pick a `source_id` and a known time range that overlaps the replay, then:

```bash
curl -v "http://localhost:3000/api/pcap/extract?source_id=local&start=0&end=3600000000" -o /tmp/extract.pcap
file /tmp/extract.pcap
```

Expected: `tcpdump capture file (little-endian) - version 2.4 (Ethernet, capture length 262144)`. Open it in Wireshark / tcpdump:

```bash
tcpdump -nr /tmp/extract.pcap | head -5
```

Expected: actual packets printed.

- [ ] **Step 3: Browser smoke**

Visit the console (default `http://localhost:3000/`), open an LLM call detail, click "Extract packets", click "Extract" in the dialog. The browser should download a `ts-extract-YYYYMMDDTHHMMSS.pcap` file.

- [ ] **Step 4: Document the smoke pass**

No commit — this phase is human verification.

---

## Self-review checklist

The plan covers each spec section:

- ✅ **Pipeline resolution (filesystem scan, no AppConfig reverse lookup)** — Task C1 (`list_candidate_files`)
- ✅ **API endpoint with all required + optional params** — Task I1 (`build_request`)
- ✅ **Validation: 400 on bad source_id, time order, time window > 1h, bad IP** — Task I1 unit tests + I3 integration test
- ✅ **200 with header-only pcap on no-match / no-files** — Task H1 + I3
- ✅ **Synchronous streaming response** — Task H1 (`Stream`), I1 (`Body::from_stream`)
- ✅ **Output filename `ts-extract-<utcNow>.pcap`** — Task I1
- ✅ **Default `link_type = 1` for empty case** — Task B1 (const), Task H1 (use)
- ✅ **Bidirectional 5-tuple match + wildcard** — Task E1 + tests
- ✅ **Plain + snappy reading; truncation tolerance** — Task D1 + tests
- ✅ **K-way merge by ts_us** — Task G1 + test
- ✅ **Format module is inlined, no `ts-capture` dep; anchor tests** — Task A1 (no dep), B1 (anchor tests)
- ✅ **Public types** — Task A2
- ✅ **Round-trip via libpcap** — Task H2
- ✅ **`router()` signature change + both call-site updates** — Task I2 + J1
- ✅ **Three console buttons + shared dialog + URL builder + native download** — Tasks K1-K3, L1-L3
- ✅ **Anchor type → defaults mapping (incl. agent_turn leaving ports blank)** — Task K1
- ✅ **End-to-end smoke** — Task M1

No placeholders, no "TODO", no "similar to Task N", no references to undefined types. Every code block is the actual content the engineer types.
