# pcap_dump: per-source dirs, minute rotation, snappy compression

**Date:** 2026-05-01
**Status:** Implemented (commits e975999..4687580; pcap_dump rewrite at 3a8c7ea)
**Scope:** `server/ts-capture/src/pcap_dump.rs`, `server/ts-capture/src/pcap_live.rs`, `server/ts-common/src/config.rs`, `server/ts-capture/Cargo.toml`

## Motivation

Today's `pcap_dump` writes one growing `.pcap` per source for the lifetime of the pipeline. Two pain points:

1. **Unbounded files** — a long-running probe accumulates a single multi-GB pcap that's painful to share, archive, or analyze for a specific time window.
2. **No compression** — pcap data is highly compressible (typical 3–5×); plain files waste disk on busy probes.

This spec adds time-based rotation, per-source directories, and optional snappy compression while preserving the existing safety guarantees (per-source error isolation, link_type pinning, error throttling, capture-pipeline never fails on dumper errors).

## Design summary

| Aspect | Current | Proposed |
|--------|---------|----------|
| Layout | `<dir>/<source>.pcap` | `<dir>/<source>/<minute>.pcap[.snappy]` |
| Rotation | None (one file per source per run) | Wall-clock minute, by packet timestamp, sparse (no empty files) |
| Compression | None | Optional snappy framed (`compression = "snappy"`) |
| Writer | `pcap::Savefile` (libpcap stdio) | Self-written pcap format → `BufWriter<File>` (or `FrameEncoder<BufWriter<File>>`) |
| Out-of-order | N/A | Forward-only ratchet; late packets go into current file + counter |
| Flush | shutdown only (live/file); on heartbeat (cloud_probe) | BufWriter natural + 1 s heartbeat (existing path, extended to live) + rotation + shutdown |

## File layout

```
data/dumps/
├── <sanitized_source_id_1>/
│   ├── 20260501T1530.pcap
│   ├── 20260501T1531.pcap
│   └── 20260501T1533.pcap.snappy   # 1532 had no packets — file is skipped
└── <sanitized_source_id_2>/
    └── 20260501T1530.pcap.snappy
```

- `<sanitized_source_id>` reuses the existing `sanitize()` rules from `pcap_dump.rs:210` (`[A-Za-z0-9._-]`, reject empty / `.` / `..`).
- Source subdirectory created lazily on first packet via `create_dir_all`.
- Filename = `YYYYMMDDTHHMM` (basic ISO 8601, UTC) computed from the packet's `timestamp_us`. Reuses the existing `days_to_ymd` helper.
- Suffix `.snappy` appended when compression is enabled.

Existing flat files from prior runs are not migrated; they remain in `<dir>/` undisturbed alongside the new per-source subdirectories.

## Writer pipeline

We **bypass `pcap::Savefile`** and write the classic pcap format ourselves. Reason: `Savefile` writes through libpcap's `FILE*`, which can't be wrapped in `snap::write::FrameEncoder`. Hand-writing the pcap format is ~40 lines and unifies the compressed and uncompressed paths.

**Global header (24 bytes), written once per file at open:**

| Field | Size | Value |
|-------|------|-------|
| `magic_number` | u32 | `0xa1b2c3d4` (microsecond resolution) |
| `version_major` | u16 | `2` |
| `version_minor` | u16 | `4` |
| `thiszone` | i32 | `0` |
| `sigfigs` | u32 | `0` |
| `snaplen` | u32 | `65535` |
| `network` | u32 | `link_type` (the source's pinned value) |

**Per-packet record (16-byte header + caplen bytes data), written for every non-heartbeat packet:**

| Field | Size | Value |
|-------|------|-------|
| `ts_sec` | u32 | `pkt.timestamp_us / 1_000_000` |
| `ts_usec` | u32 | `pkt.timestamp_us % 1_000_000` |
| `caplen` | u32 | `pkt.caplen` |
| `origlen` | u32 | `pkt.wirelen` |

Then `pkt.data` bytes.

All multi-byte fields are written **little-endian** regardless of host byte order, matching the canonical pcap-on-disk convention used elsewhere in this repo (see `pcap_file.rs:172` test fixture and the `to_le_bytes()` calls there).

**Output stack:**

| Mode | Stack |
|------|-------|
| `none` | `BufWriter<File>` (64 KiB capacity) |
| `snappy` | `snap::write::FrameEncoder<BufWriter<File>(64 KiB)>` |

Snappy framing (RFC-style frame stream, `.sz` shape) is streamable: the file is well-formed at any frame boundary, and dropping `FrameEncoder` finalizes the trailing frame.

## Rotation

Per-source state:

```rust
struct SourceWriter {
    src_dir: PathBuf,                 // <root>/<source_id>/
    link_type: u32,                   // pinned at first packet (existing semantics)
    current: Option<MinuteFile>,
}

struct MinuteFile {
    minute_key: i64,                  // pkt.timestamp_us / 60_000_000
    sink: Box<dyn Write + Send>,      // BufWriter<File> or FrameEncoder<BufWriter<File>>
}
```

`PacketDumper::write(pkt)` flow:

1. Skip heartbeats (early return — existing behavior).
2. Skip if `pkt.source_id` is in `disabled` set.
3. Resolve / lazily create `SourceWriter`; `mkdir -p <src_dir>`; pin `link_type` if first packet for this source.
4. Reject `pkt.link_type != source.link_type`: drop + throttled warn (existing behavior). The check is per source, not per file.
5. Compute `pkt_minute = pkt.timestamp_us / 60_000_000`.
6. Determine target file:
   - No `current`: open new file at `pkt_minute`.
   - `pkt_minute > current.minute_key`: drop `current` (closes / finalizes), open new file at `pkt_minute`.
   - `pkt_minute < current.minute_key` (out-of-order): increment `CaptureDumpLateMinutePackets`, write into `current`.
   - `pkt_minute == current.minute_key`: write into `current`.
7. Write 16-byte record header + `pkt.data` to `current.sink`.

`current.minute_key` is a **ratchet** — it only advances. The file label corresponds to the largest minute observed for that source so far.

## Out-of-order policy

**Forward-only ratchet.** Once a source advances to minute N, packets with timestamps in minute N-1 (or earlier) are written into N's file and counted as `CaptureDumpLateMinutePackets`. Their pcap record timestamps preserve the original `timestamp_us` — only the file label is approximate.

Rationale:
- Cross-minute reordering is rare in practice (live capture window ≈ µs; cloud-probe skew bounded by clocks).
- pcap dump is a forensic / debugging tool; preserving the packet matters more than strict file-name accuracy.
- Holding multiple files open per source to chase late arrivals is complex — snappy in particular doesn't support reopen-and-append cleanly.

If `CaptureDumpLateMinutePackets` grows materially in production, revisit with a "last-N minutes open" approach (deferred — see Out of scope).

## Flush

Three triggers, **no new timer in the dumper**:

1. **BufWriter natural fill** — automatic when 64 KiB capacity is reached.
2. **1 s heartbeat** — `dumper.flush_all()` invoked from each source's heartbeat emission site:
   - `cloud_probe.rs` — already does this (no change).
   - `pcap_live.rs` — add at both heartbeat emission paths (real-packet branch around line 199-207, idle `TimeoutExpired` branch around line 256-268).
   - `pcap_file.rs` — **no change**. File replay has no heartbeat emission of its own; final `flush_all()` on EOF (already present at `pcap_file.rs:115`) is sufficient.
3. **Rotation / shutdown** — dropping `MinuteFile` flushes BufWriter and finalizes the trailing snappy frame; `flush_all()` on shutdown does the same for all open writers.

`flush_all()` walks `writers: HashMap<source_id, SourceWriter>` and flushes each open `current.sink`. For snappy mode, `FrameEncoder::flush()` forces an unfinished frame block to be emitted — sacrificing a small amount of compression to bound crash-loss to ~1 s of data.

The heartbeat interval is `HEARTBEAT_INTERVAL_US = 1_000_000` (1 s) in `heartbeat.rs:13`. Reusing it gives the desired ~1 s flush cadence without introducing a separate timer.

## Configuration

```toml
[pipeline.pcap_dump]
enabled = false           # default off
dir = "data/dumps"        # base directory
compression = "none"      # NEW: "none" | "snappy"
# filename_template REMOVED — path is fixed: <dir>/<source>/YYYYMMDDTHHMM.pcap[.snappy]
```

`PcapDumpConfig` (in `ts-common/src/config.rs:622`):

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PcapDumpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_pcap_dump_dir")]
    pub dir: String,
    #[serde(default)]
    pub compression: PcapCompression,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PcapCompression {
    #[default]
    None,
    Snappy,
}
```

`PacketDumperConfig::from_config(...)` carries `compression` through to the dumper.

`filename_template` and the helper `default_pcap_dump_template()` are removed. Existing TOML files that still set `filename_template` deserialize fine — serde ignores unknown fields by default.

## Errors and metrics

Existing semantics preserved:
- All I/O errors counted into `CaptureDumpErrors`, throttled `tracing::warn!` (5 s window, `ThrottledWarn`).
- Source added to `disabled` set on open failure → silent for the rest of the run.
- Capture pipeline never fails on behalf of the dumper.

Metric additions:
- `CaptureDumpLateMinutePackets` — incremented per packet whose `pkt_minute < current.minute_key`.

No new metrics for bytes / files / rotations (YAGNI; can be added when an operator concretely needs them).

## Dependencies

Add to `server/ts-capture/Cargo.toml`:

```toml
snap = "1"
```

Pure-Rust, no FFI. Self-contained in `ts-capture`; not promoted to workspace dependencies.

## Tests

Retain all existing tests in `pcap_dump.rs` and adapt to the new directory layout where applicable (e.g., `round_trip_two_sources` now reads from `<dir>/a/<minute>.pcap`).

New tests:

| Test | Asserts |
|------|---------|
| `rotate_on_minute_boundary` | Two packets at `T+59s` and `T+61s` produce two files in adjacent minute names |
| `late_packet_writes_to_current_file` | After advancing to minute N+1, a packet stamped in N goes into N+1's file; `CaptureDumpLateMinutePackets` = 1 |
| `snappy_round_trip` | `compression = "snappy"` produces `.pcap.snappy`; `snap::read::FrameDecoder` + `pcap::Capture::from_file` reads identical packets |
| `per_source_directory_layout` | Two sources produce two subdirectories, each containing only their own minute files |
| `flush_makes_data_visible_before_close` | After `write` + `flush_all`, packets are readable before the dumper is dropped (validates 1 s heartbeat actually persists data) |
| `link_type_pinned_per_source_across_files` | Same source, two minutes — link_type stays pinned across file rotation; mismatched packet still dropped |

Tests for `sanitize`, empty source-id, and `days_to_ymd` are unchanged.

## Out of scope

- pcap-NG output (no operator need yet).
- Other compression algorithms (zstd, gzip).
- Indexing or building a packet-time index.
- Migrating old flat `<dir>/<source>.pcap` files from prior runs.
- Configurable rotation interval (fixed at 1 minute by user request — "不需要配置").
- Configurable BufWriter capacity (64 KiB hard-coded).
- "Last-N minutes open" out-of-order strategy (revisit only if `CaptureDumpLateMinutePackets` shows material drift in production).

## Estimated change size

~350 lines of Rust (production + tests), confined to:
- `server/ts-capture/src/pcap_dump.rs` — most of the change (writer abstraction, rotation, hand-written pcap format).
- `server/ts-capture/Cargo.toml` — add `snap = "1"`.
- `server/ts-common/src/config.rs` — `PcapDumpConfig`: add `compression`, remove `filename_template`; introduce `PcapCompression` enum.
- `server/ts-common/src/internal_metrics.rs` — register `Metric::CaptureDumpLateMinutePackets`.
- `server/ts-capture/src/pcap_live.rs` — two `dumper.flush_all()` calls at heartbeat-emit sites.
- `server/config/default.toml` — update commented sample (`compression`, drop `filename_template`).
