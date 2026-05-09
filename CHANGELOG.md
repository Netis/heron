# Changelog

All notable changes to TokenScope are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] — 2026-05-09

### Capture

- `pcap_dump` now writes to `<dir>/<sanitized_source_id>/<minute>.pcap[.snappy]`
  (per-source subdirectory + wall-clock minute rotation, sparse). Old flat
  `<dir>/<source>.pcap` files from prior runs are not migrated and remain
  alongside the new layout. **Breaking** for operators relying on the old
  flat path.
- `pcap_dump` snappy framed compression added (`compression = "snappy"`);
  writes `.pcap.snappy` files. Decompress with `snzip -d` before opening
  in Wireshark.
- `pcap_dump` per-pipeline retention sweeper enforcing age and total-size
  caps; old minute-files are pruned in the background.
- New internal metric `dump_late_minute_pkts` (capture group): incremented
  when an out-of-order packet's timestamp falls in an earlier minute than
  the file currently being written. Late packets ratchet forward into the
  current file (timestamps preserved inside the pcap record).
- Cloud-probe dumper now flushes on every heartbeat, matching pcap-live's
  `~1s crash-loss horizon` on hard termination.
- pcap-live ring buffer raised to 16 MiB to reduce kernel-side drops under
  bursty traffic.
- New `ts-pcap-extract` crate: read-side filtered extraction over rotated
  `pcap_dump` directories (powers the new `/api/pcap/extract` endpoint).

### LLM wire-API support

- Gemini AI Studio (`gemini-aistudio`) decoded end-to-end — request +
  streaming response parsing and agent-turn assembly via `GenericProfile`.
  Persisted identifier follows the `<vendor>-<surface>` form to leave room
  for future Vertex / Gemini variants.
- Tiktoken fallback estimator fills `prompt_tokens` / `completion_tokens`
  for rows where the upstream response omitted usage.

### Agent turn tracking

- `HermesProfile` (Open WebUI / Hermes-style chat clients) detected via
  body fingerprint, alongside the existing Claude CLI / Codex CLI /
  generic / OpenClaw profiles.
- Strict `session_id` anchor extraction with profile-match gating —
  `session_id` is no longer populated for wire bodies that don't satisfy
  the matched profile's shape.
- System-prompt + time-bucket `session_id` fallback for helper / one-shot
  calls that lack a stable client anchor.
- In-progress agent-turn visibility: in-memory registry exposes turns
  before they finalize, so the API and console can show the current turn
  and its calls in real time.

### Metrics

- `ttft_ms` is no longer populated for non-streaming responses (was
  previously emitted with misleading values derived from full-response
  arrival time).

### Storage

- Default `flush_interval_ms` lowered from 1000ms → 200ms — fresher data
  on the console with negligible write-amplification cost.
- Retention defaults aligned: turns ≤ calls (turns can never reference a
  pruned call).
- `ts-storage` split into an abstraction crate (`ts-storage`) plus a
  DuckDB implementation crate (`ts-storage-duckdb`); future PostgreSQL /
  ClickHouse implementations follow the same per-backend-crate pattern.
  No runtime behavior change.

### API

- `GET /api/pcap/extract` — filtered packet extraction from `pcap_dump`,
  returns a downloadable `.pcap`.
- In-progress turn detail endpoint returns the live calls list as the
  turn is still accumulating.

### Console

- Packet-extract dialog wired into traffic / call / turn detail pages
  (downloads filtered `.pcap` via the new API).
- Raw / Tree toggle for HTTP-exchange detail body viewer.
- Auto-refresh defaults to 5s and is now "quiet": previous data is kept
  while the next fetch is in flight, and time-series chart animation is
  disabled — no flicker on tick.
- In-progress agent turn surfaced in the turn list and detail view with
  its live call list.

### Configuration

- `[pipeline.pcap_dump]`: `filename_template` removed, `compression` added
  (`"none" | "snappy"`). Stale `filename_template` keys in existing TOML
  are silently ignored by serde. **Breaking** if you scripted output paths
  off the template.
- `[pipeline.queues]` keys renamed to align with the health-page metric
  names (paired `(received, dropped)` counters now share a single
  `<destination>_*` prefix). **Breaking** for hand-written TOML —
  update any custom queue-depth tuning to the new key names.

### Operations

- TUI `dbview` removed; the web console covers all of its surfaces.
- Self-hosted-runner GitHub Actions workflow added for CI.
- `libduckdb-sys` debug info stripped in dev / test profiles, cutting
  `target/` size and link time on incremental rebuilds.

## [0.1.0] — 2026-04-30

Initial release of TokenScope — an LLM API performance monitoring system that
analyzes plaintext HTTP traffic on the LLM provider's server side to measure
and diagnose inference performance.

### Capture

- libpcap-based local NIC capture with optional per-pipeline pcap file dump
- Remote packet ingestion via ZMQ from [cloud-probe](https://github.com/Netis/cloud-probe)
- Default snaplen 262144 to fit GSO super-frames
- Data-driven heartbeat emission per stream with paired received/dropped counters
- Graceful shutdown on SIGTERM/SIGHUP with pcap dump flush and `pcap_breakloop`-based cancel

### Protocol parsing

- L2–L4 + HTTP/1.1 + SSE parsing (zero-copy via `httparse`)
- Per-direction TCP reassembler with out-of-order segment reorder buffer
- Forced resync on snaplen-truncated TCP segments and HTTP boundaries
- Silent-drop observability for the reassembler

### LLM wire-API support

- OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, Gemini, vLLM, Ollama
- Two-pass detection via `RouteVerdict` + `matches_shape`
- Generic agent profiles (`generic-anthropic`, `generic-openai-chat`, `generic-openai-responses`) for arbitrary clients hitting standard endpoints
- OpenClaw agent profile
- Strict per-endpoint OpenAI parsers (Chat vs Responses split)
- Shared SSE event-JSON parsing across extractors
- Anthropic SSE accumulator keyed by content block index

### Agent turn tracking

- Agent profile registry with state-machine-based turn boundary detection
- Buffer-and-finalize tracker with per-stream event-time watermark + wall-clock grace
- Explicit/implicit ingest path split
- Client/server IP tracking and filters on agent turns
- Tool-use ↔ tool-result join across calls within a turn

### Metrics

- TTFT, E2E Latency, TPOT, Call Rate, Token Throughput, Active Calls, Call Error Rate, Cache Hit Ratio
- Sliding-window aggregation with `*_sum` + `*_count` pairs (mean derivable client-side)
- Dimension filters (`wire_api`, `agent_kind`, `model`, `source_id`, etc.) applied to summary / models / timeseries queries
- Internal-metrics observability: active-flows / active-turn gauges, queue-depth gauges, packet-drop counters, leak-canary gauges

### Storage

- Pluggable backend trait with three implementations:
  - DuckDB (default, embedded, single-file)
  - PostgreSQL
  - ClickHouse
- Three entities: `agent_turns`, `llm_calls`, `llm_metrics` (+ `llm_finish_metrics`, `http_exchanges`)
- Per-table retention enabled by default with sane TTLs
- No-JOIN read-path rule — cross-entity reads split into PK lookups
- Raw HTTP exchange persistence via joiner stage

### API

- Axum REST API + WebSocket
- Agent sessions endpoints
- Per-page dimension filter spec
- Source ID surfaced end-to-end (renamed from `stream_id`)

### Console (React + TypeScript + shadcn/ui + Tailwind)

- Pages: traffic, LLM calls, agent turns, agent sessions (list + transcript), pipeline health
- LLM call detail with structured I/O renderer per `wire_api` × `agent_kind`
- Agent turn detail organized as behavior narrative with tool_id index fusion
- Raw HTTP drawer with Tree/Raw body viewer
- Filters: `agent_kind`, `client_ip`, URI-contains, errors-only
- Sidebar state + refresh URL param persistence

### Operations

- `install.sh` with XDG-aware config discovery cascade, `--help`, sudo+user-dir guard, OS-specific next steps
- GitHub Actions release workflow (macOS x86_64 + arm64 on `macos-14` runners)
- Demo flow (SSH/setup + cross-compile + deploy) via `just demo`
- VERSION-file SSOT pattern with `just bump` syncing `Cargo.toml` + `package.json`
- LICENSE + public README + glossary + mission doc
