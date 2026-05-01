# pcap-extract: filtered packet download from pcap_dump

**Date:** 2026-05-02
**Status:** Proposed
**Scope:** new crate `server/ts-pcap-extract`, new route `server/ts-api/src/routes/pcap_extract.rs`, new console feature `console/src/features/pcap-extract/`, minor wiring in `server/app/tokenscope/src/main.rs`.

## Motivation

`pcap_dump` (commits `8c0cce5`, `ebdb654`) writes every captured non-heartbeat packet to disk under `<base>/<pipeline>/<source_id>/YYYYMMDDTHHMM.pcap[.snappy]`. Operators today have no UI / API to pull a slice back out — they have to SSH into the host, find the right minute file(s), and filter by hand with `tcpdump -r` or Wireshark.

Two concrete debugging workflows surface this gap:

1. **HTTP exchange that should be an LLM call but isn't recognized** — the operator sees a row on the HTTP exchange page, suspects a parser miss or a wire-API edge case, and wants the surrounding packets to inspect headers / payload bytes / TLS framing in Wireshark.
2. **Many `llm_calls` rows but very few `agent_turns`** — the operator suspects turn aggregation broke down (session boundary detection, time gap heuristics) and wants to replay the actual on-the-wire traffic that produced this discrepancy.

Generalizing both: starting from any row that exposes a 5-tuple + time, let the operator pull a filtered, time-bounded `.pcap` slice that opens directly in Wireshark.

## Design summary

| Aspect | Decision |
|--------|----------|
| Public API | `GET /api/pcap/extract?source_id=…&start=…&end=…[&client_ip=…&client_port=…&server_ip=…&server_port=…]` |
| Output format | Single uncompressed classic-pcap stream; snappy sources transparently decoded |
| Time-window cap | Hard ≤ 1 hour (server returns 400 above) |
| Filter semantics | Empty 5-tuple field = wildcard; matches both directions of the flow; non-IP and non-TCP packets dropped (matches the rest of TokenScope, which is TCP-only) |
| Pipeline resolution | No schema migration. `AppConfig` enumerates `(pipeline_name, pcap_dump.dir)`; for each, scan `<dir>/<pipeline>/<source_id>/`. Multi-pipeline same `source_id` → all candidates merged. |
| Output filename | `Content-Disposition: attachment; filename="ts-extract-<utcNow>.pcap"` (system time at request, second resolution) |
| Concurrency model | Synchronous streaming response — k-way merge yields packets in time order, body is `tokio_util::io::StreamReader` over the merge stream |
| UI surface | Modal dialog on three detail pages: HTTP exchange, LLM call, agent turn. Default values prefilled from row; user-editable. |

## Pipeline resolution (filesystem scan, no AppConfig reverse lookup)

`CaptureSourceConfig::resolved_source_id()` returns `None` for cloud-probe sources — `source_id` comes from the ZMQ batch UUID at runtime, never from static config. So we cannot reverse-lookup `source_id → pipeline_name` from `AppConfig.pipelines[].capture.sources[]`.

**Resolution algorithm:**

```
roots = AppConfig.pipelines.map(|p| (p.name, p.pcap_dump.dir))
for (pipeline_name, base_dir) in roots:
    src_dir = base_dir / sanitize(pipeline_name) / sanitize(source_id)
    if src_dir.is_dir():
        candidate_dirs.push(src_dir)
```

- All static-source path math reuses existing helpers (`sanitize` lives in `ts_common::path`; `pcap_dump_dir_for` already composes `base / pipeline_name`).
- A pipeline whose `pcap_dump.enabled = false` typically has no directory tree → it falls through naturally without a per-config check.
- If `candidate_dirs` is empty, the request still returns 200 OK with a header-only pcap (uniform success contract; explained under "Error matrix").

**Multi-pipeline same `source_id`** — extremely rare in production (pipeline names are deployment-scope identifiers and source_ids are usually globally unique by accident — NIC names, probe UUIDs). When it happens, all candidates are scanned and merged into one time-ordered output. This is consistent with the operator intent ("show me everything that flowed through this source_id").

**Historical pipeline rename** — out of scope. Only currently configured pipelines are scanned. If an operator renames a pipeline, old files at the old name become invisible to extract; they have to rename back or re-point a config. Documented under Out of scope.

## API

### Endpoint

```
GET /api/pcap/extract
```

### Query parameters

| Name | Required | Type | Notes |
|------|----------|------|-------|
| `source_id` | ✓ | string | Must pass `ts_common::path::sanitize_path_component` |
| `start` | ✓ | i64 | Microseconds since epoch (matches existing `/api/llm-calls?start=`) |
| `end` | ✓ | i64 | Microseconds since epoch |
| `client_ip` | – | string | IPv4 or IPv6; absent or empty = wildcard |
| `client_port` | – | u16 | Absent or empty = wildcard |
| `server_ip` | – | string | IPv4 or IPv6; absent or empty = wildcard |
| `server_port` | – | u16 | Absent or empty = wildcard |

### Validation (returns 400 on failure)

- `source_id` fails `sanitize_path_component`
- `start >= end`
- `end - start > 3_600_000_000` (1 hour, in microseconds)
- `client_ip` / `server_ip` does not parse as `IpAddr`
- `client_port` / `server_port` not in `0..=65_535` (serde `u16` already enforces; explicit check covers stringly-typed empty-vs-absent)

### Success response

- `200 OK`
- `Content-Type: application/vnd.tcpdump.pcap`
- `Content-Disposition: attachment; filename="ts-extract-<utcNow>.pcap"` where `<utcNow>` is `YYYYMMDDTHHMMSS` of the request handler's wall clock
- Body: streaming bytes — exactly one classic-pcap global header followed by zero or more pcap records in strictly ascending `ts_us` order

When no candidate directories exist, or none of the records in covered minute files match the filter, the body is a 24-byte header-only pcap. This is a valid pcap (`Capture::from_file` opens it as "0 packets") and gives a uniform success contract regardless of whether `pcap_dump` is enabled, files were already reaped by retention, or the filter simply matched nothing.

The `link_type` field in this header-only case is `1` (DLT_EN10MB / Ethernet). LLM-relevant traffic captured on the server side is almost always Ethernet; an empty Ethernet-link-type pcap opens cleanly in Wireshark and shows "0 packets". When at least one candidate file exists, its `link_type` is used instead.

### Error matrix

| Status | Cause |
|--------|-------|
| 400 | Param validation failed (any of the above) |
| 500 | Unexpected I/O error mid-stream; or two candidate files report different `link_type` (extremely unlikely — pcap_dump pins link_type per source_id) |

No 404 / 503 — see "Pipeline resolution" rationale.

### Output filename

System time of the request, not the extracted-window time. Operators identify saved files by when they ran the extract, not by what window they pulled. Format `YYYYMMDDTHHMMSS` (UTC, no separators) keeps lexical-sort = chronological-sort.

## Extraction engine — `ts-pcap-extract`

### Public surface

```rust
pub struct ExtractRequest {
    pub source_id: String,
    pub start_us: i64,
    pub end_us: i64,
    pub client_ip: Option<IpAddr>,
    pub client_port: Option<u16>,
    pub server_ip: Option<IpAddr>,
    pub server_port: Option<u16>,
}

pub struct PipelineRoot {
    pub name: String,        // raw pipeline name; sanitize() is applied internally
    pub dump_dir: PathBuf,   // pipeline.pcap_dump.dir
}

pub fn extract(
    req: ExtractRequest,
    roots: &[PipelineRoot],
) -> impl Stream<Item = io::Result<Bytes>> + Send;
```

The stream emits ready-to-write byte chunks: first the 24-byte global header, then one chunk per matching pcap record (16-byte record header + caplen bytes data). The HTTP handler wraps it in `tokio_util::io::StreamReader` and feeds it directly to `axum::body::Body::from_stream(...)`.

### Internal modules

```
ts-pcap-extract/src/
├── lib.rs           # ExtractRequest, PipelineRoot, extract()
├── format.rs        # PCAP_MAGIC / PCAP_VERSION_* / PCAP_SNAPLEN consts;
│                    # minute_label, parse_minute_label; anchor tests
├── candidates.rs    # filesystem scan: list <root>/<pipeline>/<source>/<minute>.pcap[.snappy]
├── reader.rs        # PacketIter over plain or snappy file; truncation-tolerant
├── filter.rs        # 5-tuple bidirectional match + time window
├── merge.rs         # k-way merge by ts_us → Stream
└── output.rs        # write global header + per-record header
```

### `format.rs` — drift control

Constants and helpers are **inlined**, not re-exported from `ts-capture`:

- `PCAP_MAGIC = 0xa1b2_c3d4`, `PCAP_VERSION_MAJOR = 2`, `PCAP_VERSION_MINOR = 4`, `PCAP_SNAPLEN = 262_144` — frozen by external pcap classic spec; will not drift.
- `minute_label(i64) -> String` and `parse_minute_label(&str) -> Option<i64>` — duplicated logic (~30 lines). Already on disk in production via `pcap_dump`, so the format is similarly frozen.

**Why no `ts-capture` dep:** The on-disk format is the actual contract — both writer and reader decode it from disk. Sharing 30 lines via a cross-crate dependency would (a) reverse the natural domain direction (read depends on write), (b) pull libpcap / ZMQ / snap / retention into every `ts-pcap-extract` build, and (c) violate Occam's Razor. Drift is prevented by **anchor tests** on both sides:

```rust
// ts-pcap-extract/src/format.rs — tests
assert_eq!(minute_label(0), "19700101T0000");
assert_eq!(minute_label(29_633_130), "20260505T1330");
assert_eq!(PCAP_MAGIC, 0xa1b2_c3d4);
```

The same fixtures already exist on the `ts-capture` side (`minute_label_format`, `global_header_is_24_bytes`). If either side mutates, its own tests go red first.

A doc comment at the top of each `format.rs` cross-references the other crate's location.

### `candidates.rs`

Walk every root, compute candidate src dirs, then enumerate minute files within `[start_us, end_us]`:

```
minute_lo = req.start_us.div_euclid(60_000_000)
minute_hi = req.end_us.div_euclid(60_000_000)
for src_dir in candidate_src_dirs:
    for k in minute_lo..=minute_hi:
        label = minute_label(k)
        for ext in [".pcap", ".pcap.snappy"]:
            path = src_dir.join(format!("{label}{ext}"))
            if path.is_file(): files.push(path)
```

A given `(src_dir, minute_key)` pair will, in normal operation, only have one suffix — pcap_dump's compression mode doesn't change mid-process. When both somehow exist (manual operator action; restart with config flip), both are read. The merge stage tolerates duplicate timestamps.

**Sparse minutes are normal** — `pcap_dump` does not create files for minutes with zero packets. Missing files are silently skipped, not errored.

### `reader.rs`

One reader per file. Plain files use `BufReader<File>`; `.snappy` files use `snap::read::FrameDecoder<BufReader<File>>`. Both expose `Iterator<Item = io::Result<RawRec>>`.

Open path:
1. Read 24-byte global header. Validate `magic == PCAP_MAGIC` (drop file with warn on mismatch).
2. Capture `link_type` for the merge stage to compare across files.
3. Loop: read 16-byte record header → caplen → wirelen → ts_sec → ts_usec; read `caplen` bytes data.

Truncation tolerance:
- EOF *between* records → end iteration cleanly.
- EOF *inside* a record header or *inside* its data → log throttled debug, end iteration cleanly. This covers the in-flight current minute file (pcap_dump's BufWriter flushes on 1-second heartbeat, so a reader can race with a writer).
- Snappy half-frames at EOF → `FrameDecoder` raises `UnexpectedEof`; treated identically.

### `filter.rs`

Per record, parse Ethernet → IP → TCP using `ts_protocol::de::decode(data, link_type, ts_us, source_id)`. Reject anything that doesn't reach TCP — ARP, ICMP, UDP/QUIC, IP fragments, malformed (`DecodeError::NotTcp`, `Truncated`, `InvalidHeader`, etc. all fall through to "skip"). This matches the rest of TokenScope, which is TCP-only today; HTTP/3-over-QUIC traffic isn't analyzable upstream either, so making extract reject it keeps the contract consistent.

Bidirectional match:

```rust
fn matches(req: &ExtractRequest, fields: &L4Fields) -> bool {
    let forward = field_match(req.client_ip,   fields.src_ip)
               && field_match(req.client_port, fields.src_port)
               && field_match(req.server_ip,   fields.dst_ip)
               && field_match(req.server_port, fields.dst_port);
    let reverse = field_match(req.client_ip,   fields.dst_ip)
               && field_match(req.client_port, fields.dst_port)
               && field_match(req.server_ip,   fields.src_ip)
               && field_match(req.server_port, fields.src_port);
    forward || reverse
}

fn field_match<T: Eq>(filter: Option<T>, actual: T) -> bool {
    filter.map_or(true, |f| f == actual)
}
```

Time window:

```rust
req.start_us <= rec.ts_us && rec.ts_us <= req.end_us
```

(inclusive both ends; matches operator intuition for "from 12:00:00 to 12:00:05")

### `merge.rs`

K-way merge using `BinaryHeap<Reverse<HeapEntry>>`. Each entry holds:

```rust
struct HeapEntry {
    ts_us: i64,
    file_idx: usize,
    rec: RawRec,
}
```

Algorithm:
1. Open all candidate files, read first matching record from each → push to heap.
2. Loop: pop smallest, emit, pull next matching record from `file_idx`, push if any.

When a file's iterator returns an `io::Result::Err`, log throttled and stop reading that file (do not abort the whole stream — partial result is more useful than failing entirely on a single corrupt file).

`link_type` consistency: the global header from each file is captured at open. The first file's `link_type` is authoritative. Any later file with a different `link_type` causes the stream to emit an error and the handler to surface 500. Should never trigger in practice (pcap_dump pins per source_id).

### `output.rs`

```rust
fn write_global_header(link_type: u32) -> [u8; 24] { ... }
fn write_record_header(rec: &RawRec) -> [u8; 16] { ... }
```

These mirror `ts-capture::pcap_dump`'s write functions byte-for-byte but live in their own module per the no-dep rationale.

## HTTP handler — `ts-api`

New file `server/ts-api/src/routes/pcap_extract.rs`. Mounted at `/api/pcap/extract` from `routes/mod.rs`.

```rust
pub async fn handler(
    State(roots): State<Arc<Vec<PipelineRoot>>>,
    Query(params): Query<ExtractParams>,
) -> Result<impl IntoResponse, ApiError> {
    let req = params.try_into_request()?;   // returns ApiError::InvalidParam on validation
    let stream = ts_pcap_extract::extract(req, &roots);
    let body = Body::from_stream(stream);
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/vnd.tcpdump.pcap"),
            (CONTENT_DISPOSITION, format!("attachment; filename=\"ts-extract-{}.pcap\"", utc_now_label())),
        ],
        body,
    ).into_response())
}
```

`ApiError` extension: add `InvalidParam(String) -> 400` if not already present (current `ApiError` enum has it per `routes/http_exchanges.rs:73` usage).

`Vec<PipelineRoot>` is built once in `server/app/tokenscope/src/main.rs` from `AppConfig.pipelines` and injected as router state alongside the existing `StorageBackend`. No per-request AppConfig read.

## Frontend — `console/src/features/pcap-extract/`

### Components

- `ExtractPacketsButton.tsx` — small button (e.g. icon + "Extract packets") that opens the dialog. Accepts an `anchor: AnchorProps` prop describing how to prefill defaults.
- `ExtractDialog.tsx` — modal with `source_id` (read-only), four 5-tuple inputs (text), two datetime-local inputs (UTC). "Extract" button disabled when `end - start > 1h` or any IP/port malformed. On submit:
  ```ts
  const a = document.createElement('a');
  a.href = `/api/pcap/extract?${qs}`;
  a.click();          // browser handles the streaming download
  closeDialog();
  ```
- `extract-defaults.ts` — pure functions mapping each anchor type → form initial values.

### Anchor types

| Anchor | `client_port` | `server_port` | start | end |
|--------|---------------|---------------|-------|-----|
| `http_exchange` | row.client_port | row.server_port | row.request_time - 1s | row.response_complete_time + 1s (fallback `request_time + 5s` if absent) |
| `llm_call` | row.client_port | row.server_port | row.request_time - 1s | row.complete_time + 1s (fallback `request_time + 5s`) |
| `agent_turn` | (empty) | (empty) | turn.start_time - 1s | turn.end_time + 1s |

The ±1s padding catches SYN / FIN / late ACKs around the application-level boundaries. agent_turn leaves both ports empty because turn-level analysis spans multiple TCP connections (different `client_port` per call, possibly different `server_port` if multiple endpoints).

### Wiring

Three detail pages each gain one line:

```tsx
<ExtractPacketsButton anchor={{ type: 'http_exchange', row }} />
```

No global state. No localStorage. No "recent extractions" history. (YAGNI.)

## Tests

### `ts-pcap-extract`

| Test | Asserts |
|------|---------|
| `format::minute_label_anchored` | Fixture strings match ts-capture-side fixtures (drift guard) |
| `format::pcap_consts_anchored` | Magic / version / snaplen literal values |
| `candidates::lists_plain_and_snappy` | Both suffixes for same minute discovered |
| `candidates::skips_missing_minutes` | Sparse minute files don't error |
| `candidates::scans_multiple_pipelines` | Same source_id under two pipelines → both candidates listed |
| `reader::truncated_record_tolerated` | EOF mid-record-header / mid-data → clean stop, prior records preserved |
| `reader::truncated_snappy_frame_tolerated` | Half-frame at EOF treated as clean stop |
| `filter::matches_both_directions` | Same 5-tuple in both src→dst and dst→src forms matches |
| `filter::any_means_wildcard` | None fields match arbitrary values |
| `filter::time_window_inclusive` | start/end boundaries included; outside excluded |
| `filter::non_tcp_dropped` | ARP / ICMP / UDP / non-IP / malformed records silently dropped |
| `merge::k_way_time_ordered` | Multi-file multi-pipeline input → output strictly ascending ts_us |
| `merge::link_type_mismatch_errors` | Two candidates with different link_type → error propagated |
| `output::header_only_when_no_match` | Empty match window → exactly 24 bytes, parseable as pcap |
| `e2e::round_trip_through_pcap_dump` | Drive pcap_dump to write fixtures → extract → libpcap `Capture::from_file` opens; record bytes byte-equal |

### `ts-api`

| Test | Asserts |
|------|---------|
| `pcap_extract::happy_path` | 200 + headers + body parses as valid pcap |
| `pcap_extract::empty_match_returns_header_only` | 200 + 24-byte body |
| `pcap_extract::no_candidate_dir_returns_header_only` | source_id with no dirs → 200 + 24 bytes |
| `pcap_extract::time_window_too_wide_400` | end - start > 1h → 400 |
| `pcap_extract::bad_source_id_400` | Sanitize failure → 400 |
| `pcap_extract::time_reversed_400` | start ≥ end → 400 |
| `pcap_extract::bad_ip_400` | Malformed IP → 400 |

### `console`

Component tests for `<ExtractPacketsButton anchor={…}/>` rendering the dialog with correct prefilled values, one fixture per anchor type. No e2e browser test.

## Dependencies

New crate `ts-pcap-extract`:

```toml
[dependencies]
ts-protocol  = { path = "../ts-protocol" }   # L2-L4 parsing for 5-tuple filter
ts-common    = { path = "../ts-common" }     # sanitize_path_component, AppError
snap         = "1"                           # framed snappy decoder
bytes        = { workspace = true }
tokio        = { workspace = true }
tokio-util   = { workspace = true }          # StreamReader
futures      = { workspace = true }
tracing      = { workspace = true }

[dev-dependencies]
tempfile     = { workspace = true }
pcap         = { workspace = true }          # for e2e round-trip via libpcap
```

`ts-api` adds `ts-pcap-extract = { path = "../ts-pcap-extract" }`.

`server/Cargo.toml` workspace `members` adds `"ts-pcap-extract"`.

No changes to `ts-capture`, `ts-storage`, `ts-common::config`, or any DDL.

## Out of scope

- **Multiple explicit 5-tuples** (POST + JSON body of N flows) — wildcard semantics on optional fields cover ~95% of intent. Add a separate `POST /api/pcap/extract:by-flows` if that gap surfaces.
- **`agent_session` anchor button** — turn-level extraction already serves "analyze one agent process". Session windows span hours and need different default-range UX; revisit if operators ask.
- **Async extraction job table + polling** — the 1-hour cap keeps synchronous streaming acceptable.
- **Output format options** (`.pcap.gz`, `.pcap.snappy`) — single uncompressed `.pcap` only. HTTP `Content-Encoding: gzip` (transfer compression) is orthogonal and can be added by the reverse proxy if desired.
- **Packet content rewriting** (IP rewrite, payload masking) — full byte-for-byte passthrough.
- **CLI tool** (`tokenscope extract …`) — the `ts-pcap-extract::extract()` core can host one later; not built for v1.
- **Cross-link_type conversion** — multi-pipeline same source_id with different link_types errors with 500. Should never trigger in practice.
- **Historical pipeline rename / orphan dir scanning** — only pipelines currently in `AppConfig` are scanned. Renamed pipelines need a config patch.
- **Auth / authz** — consistent with current API: none. Network-level isolation in the deployment is assumed.
- **Schema migration** (adding `pipeline_name` to llm_calls / http_exchanges / agent_turns) — not needed; filesystem scan resolves pipeline.
- **Standalone `/pcap-extract` console page** — only modals on detail pages.
- **Recent-extractions history / localStorage** — not stored.
- **Rate limiting on the endpoint** — consistent with rest of API.

## Estimated change size

- `server/ts-pcap-extract/` (new crate, including tests): ~800–1000 lines
- `server/ts-api/src/routes/pcap_extract.rs` (new) + `routes/mod.rs` registration: ~200 lines
- `server/app/tokenscope/src/main.rs` (compute and inject `Vec<PipelineRoot>`): ~30 lines
- `server/Cargo.toml` workspace member: 1 line
- `console/src/features/pcap-extract/` (new): ~250 lines TSX
- Three detail pages: 1 line each

Total ≈ 1500 lines Rust + 300 lines TSX, fully within a single implementation pass.
