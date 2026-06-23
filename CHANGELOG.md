# Changelog

All notable changes to Heron are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] — 2026-06-24

### Changed

- **OpenTelemetry-aligned rename of the storage entities and HTTP API.** The
  `agent_turns` table is now `traces`, `llm_calls` is now `spans`, and
  `traces.call_ids` is now `span_ids`; a new forward-looking `spans.kind`
  column (always `'llm'` today) leaves room for wire-visible tool spans. The
  Rust domain/query types (`AgentTurn`→`Trace`, `Turn*`/`Call*` DTOs →
  `Trace*`/`Span*`) and `StorageBackend` methods (`write_calls`→`write_spans`,
  `query_turns`→`query_traces`, …) follow suit. New canonical routes
  `/api/traces*` and `/api/spans*`; the pre-rename `/api/agent-turns*` and
  `/api/llm-calls*` keep working as deprecated aliases (RFC 8594 `Deprecation`
  header). Retention config keys `calls`/`turns` are accepted as serde aliases
  for `spans`/`traces`. Existing DuckDB/ClickHouse databases auto-migrate in
  place on init() with no data loss (idempotent detect-then-rename).

### Docs

- **README rebuilt for the launch — GIF-first, conversion-driven layout.**
  Headline tagline "The Wireshark for AI Agents", a hero demo GIF up top, a
  30-second pcap-replay quick start, and a three-up "What Makes Heron Different"
  table (agent-turn reconstruction · service topology · SFT trajectory export),
  while preserving the existing technical accuracy (passive positioning across
  wire *and* on-host TLS boundary, opt-in eBPF, `docs/configure.md` links,
  `just`-based contributor flow). API examples updated to the canonical
  `/api/traces` routes.
- **Corrected launch collateral added under `launch/`.** Paste-ready Product
  Hunt / Hacker News / Twitter copy aligned to the shipped product: install via
  the `curl … install.sh` one-liner (no npm package), canonical `/api/traces`
  endpoint, and v0.7.0 framing.

## [0.6.0] — 2026-06-16

### Changed

- **eBPF on-host SSL-uprobe capture is now a first-class, soak-gated
  capability** (experimental since 0.5.1). It lifts the plaintext of
  TLS-encrypted LLM calls directly at the in-process `SSL_read` / `SSL_write`
  boundary — covering dynamically-linked OpenSSL/BoringSSL (Python `openai` /
  `anthropic` SDKs, curl, Node, most CLIs) and statically-linked, symbol-stripped
  BoringSSL single-executable runtimes (Claude Code's / opencode's Bun binaries,
  located by byte-signature offset) — and stamps every call with its owning
  process (pid · command · executable). A new staging `ebpf-soak` gate replays
  real TLS traffic through the freshly deployed binary and asserts the uprobe
  attaches, traffic is captured, and a process-attributed `LlmCall` is parsed and
  persisted end-to-end; both prod promotion and release now require a passing
  `ebpf-soaked` status alongside `staging-soaked`, so on-host capture can no
  longer silently regress into prod or a cut release.

### Fixed

- **eBPF SSL uprobes attach under the non-root staging service.** The staging
  unit granted only `CAP_BPF` + `CAP_PERFMON`, but the kernel gates uprobe
  `perf_event_open` (`perf_uprobe_init`) on `CAP_SYS_ADMIN` specifically — which
  those caps don't cover and `perf_event_paranoid` doesn't relax. The `SSL_write`
  uprobe attach therefore failed with `perf_event_open failed`, and because a
  failed capture source is non-fatal (the co-located packet tap keeps the
  pipeline healthy) the symptom was a silent `ebpf_uprobes_attached = 0` rather
  than a crash — which is why the `ebpf-soak` gate had never passed. The
  committed staging unit now carries `CAP_SYS_ADMIN` (matching what the prod
  deploy already injects), so the gate goes green and on-host capture works under
  a non-root service.

### Docs

- **README repositioned from "network wire" to passive capture.** The headline
  "Agent observability from the network wire" became inaccurate once on-host eBPF
  landed — those bytes never touch the wire (on a client they're the
  pre-encryption plaintext). It now leads with the durable differentiator
  (passive; no SDK, sidecar, or proxy; never in the request path) and names both
  capture surfaces: off the network wire, or lifted from the host's TLS boundary
  by eBPF.

## [0.5.6] — 2026-06-16

### Fixed

- **Claude Code's security-monitor sidecar no longer masks/floods agent-turns.**
  Claude Code fires a background `/v1/messages` "security monitor" call that
  feeds the running transcript to a supervisor prompt and returns a tiny
  `<block>yes/no` verdict (system prompt "You are a security monitor for
  autonomous AI coding agents", `stop_sequences=["</block>"]`, no `tools`
  field). Because it embeds the transcript, it synthesized the *same* session
  anchor as the real conversation, so the turn tracker merged it into the real
  turn and overwrote that turn's answer with "<block>no" (and standalone ones
  flooded the view) — on prod ~73% of claude-opus turns were this housekeeping
  noise, masking real working sessions. The `claude-cli` profile's
  `is_auxiliary` (which already drops one-shot sidecars from turn tracking while
  keeping the call in `llm_calls`) only caught `tools:[]`; the monitor call has
  no `tools` field, so it slipped through. It is now flagged by its
  system-prompt signature, so real conversation turns form cleanly from real
  calls only.

## [0.5.5] — 2026-06-16

### Fixed

- **Anthropic control-plane telemetry no longer recorded as `model=unknown` LLM
  calls.** Claude Code stamps the `anthropic-version` header on *all* its
  api.anthropic.com POSTs, including control-plane telemetry
  (`/v1/code/.../worker/events`) that carries no `{model, messages}` body. The
  Anthropic route classifier had a header-only fallback that accepted any
  anthropic-header request on a non-canonical path, short-circuiting the body
  check — so that telemetry was stored as a `model=unknown` Anthropic call.
  Harmless before, but the per-inode eBPF rescan (0.5.4) now captures whole
  interactive sessions, so these flooded the calls view. The header-only
  non-canonical path now defers to the shape pass, which requires an inference
  body and treats the anthropic header / `sk-ant-` key as the vendor
  disambiguator: telemetry (no model/messages) is dropped while real inference —
  canonical `/v1/messages` *and* gateway-rewritten paths — still classifies.

## [0.5.4] — 2026-06-15

### Fixed

- **eBPF capture follows binary auto-updates and reaches already-running
  sessions.** uprobes are installed per *inode*, but npm-style auto-updates
  (Claude Code, opencode) stage a new build in a `.<pkg>-<hash>/` dir and
  atomically rename it over the install path. The loader attached once at
  startup to whichever inode was on disk then, so it silently missed both
  already-running sessions (which keep an unlinked "(deleted)" inode) and every
  session started after an update (which execs the new on-disk inode) — a
  long-lived Claude Code session was captured by neither. Attach is now per
  distinct inode and re-scanned every 15s: the loader enumerates the on-disk
  install path *and* every running target process via `/proc/<pid>/exe` (which
  the kernel resolves to the real inode even when deleted), so probes track
  inode rotation and reach live sessions without a service restart. Steady-state
  cost is a `stat` per candidate; the binary read + signature scan runs only for
  inodes never seen before.

## [0.5.3] — 2026-06-15

### Fixed

- **eBPF capture of large and multi-connection TLS traffic.** A big `SSL_*`
  buffer is split into several ring-buffer events; the synthesizer used to append
  each at a running sequence counter, so a silently-dropped or reordered chunk
  shifted every later byte and spliced the next request's bytes into the previous
  body (invalid JSON → no captured call). The BPF program now stamps each event
  with its absolute per-`(connection, direction)` stream offset and the
  synthesizer places every chunk at that offset, so a dropped chunk leaves a gap
  at its true position instead. Connections also finalize properly now — an
  `SSL_free` uprobe (dynamic libssl) and an `SSL_read`-returns-0 EOF (static
  targets) emit a close, and a per-connection generation folded into the
  synthetic tuple gives a reused `SSL*` pointer a fresh, non-overlapping flow
  key.
- **Claude Code conversations form agent-turns.** The Anthropic
  `mid-conversation-system` feature appends a trailing `role=system` notice after
  the user's prompt, so the messages array often ends with `system` on a fresh
  turn. `is_user_turn_start` / `extract_user_input` now skip trailing system
  messages and evaluate the last non-system one, so fresh turns are recognized
  (and their prompt preview populated) instead of being discarded as
  "no user start".
- **In-progress turn timestamps rendered as the year ~58423.** Active-turn
  registry rows emitted `start_time`/`end_time` in microseconds, but the field is
  milliseconds (the DB path returns `epoch_ms`, the console renders with
  `new Date(ms)`). A µs value read as ms is 1000× too far in the future. Both
  active-registry conversions now divide to milliseconds, matching finalized
  turns. Added a timestamp-unit test suite across the eBPF clock and the API
  boundary.

## [0.5.2] — 2026-06-11

### Added

- **eBPF capture is now a managed Settings source.** The console's pipeline
  Settings lists, adds, edits, and removes the on-host eBPF SSL-uprobe source
  like any other ingress, with an availability guard: a binary built without the
  `ebpf` feature reports `ebpf_available = false` on `/api/runtime-config`, and
  the API rejects an eBPF source on such a build so a stray config can't wedge
  the next boot. (The capture engine itself shipped in 0.5.1; this makes it
  operable from the UI.)
- **eBPF metrics in pipeline-health.** A new `ebpf` metric group surfaces the
  capture path's health — uprobes attached, events received / dropped, bytes
  captured, frames synthesized, active connections, and process-cache size — in
  the debug pipeline-health view and on `/api/internal-metrics`.

### Changed

- **Console theme aligned to the Kami parchment design system.** Replaced the
  too-dark palette with the canonical parchment surfaces (paper canvas, lifted
  ivory, ink-blue accent), matching the heron-ai.pages.dev landing site.

### Docs

- Refreshed the README screenshots in the corrected Kami theme.

## [0.5.1] — 2026-06-11

### Added — eBPF on-host TLS capture (experimental, Linux)

- New `ebpf` capture source: a fourth ingress alongside the packet taps
  (`pcap` / `pcap-file` / `cloud-probe`). It attaches uprobes to the target's
  `SSL_read` / `SSL_write` and reads plaintext at the in-process TLS boundary —
  so Heron can observe **TLS-encrypted** LLM calls *on the host that makes them*,
  with no proxy, TLS terminator, or MITM, and nothing on the request path.
  Plaintext chunks are dressed as synthetic Ethernet/IP/TCP frames
  (`FlowSynthesizer`) and fed through the existing dispatcher → reassembler →
  HTTP/SSE parser → wire-API decoder → turn tracker unchanged.
- **Process attribution.** Every eBPF-captured call carries its owning process
  (`pid` · `comm` · resolved executable), threaded end-to-end through
  `RawPacket` → `ParsedPacket` → `TcpFlow` → `LlmCall` into the
  `process_pid` / `process_comm` / `process_exe` storage columns (DuckDB Phase-7
  migration + ClickHouse mirror) and surfaced in the console's LLM-calls list and
  call detail. Packet-tap sources leave it null.
- **Target coverage.** Dynamically-linked OpenSSL/BoringSSL by exported symbol
  (Python SDKs, curl, …), and statically-linked, symbol-stripped BoringSSL —
  e.g. Claude Code's Bun runtime — located by byte-signature → ELF file offset →
  offset uprobe. A built-in `flavor = "bun"` ships read-anchored prologue
  signatures, so stock Bun / Claude Code works with zero manual derivation.
- Linux-only and **off by default**: built behind the non-default `ebpf` cargo
  feature on `h-capture` (absent from prebuilt release binaries). Needs
  `CAP_BPF` + `CAP_PERFMON` (kernel ≥ 5.8) or root, plus kernel BTF; `heron
  doctor` reports a `capture.ebpf` check. HTTP/1.x only, like every source. See
  `docs/design/02-capture.md` and `docs/design/03-ebpf-static-targets.md`.

### Added

- **SFT trajectory export** from reconstructed agent turns and sessions:
  OpenAI-style `messages` JSONL with tool calls, tool results, and assistant
  reasoning preserved and tool-call arguments rehydrated to objects. Export a
  single turn/session from its detail view, or batch-export the current Agent
  Turns filter as one-line-per-turn JSONL (Anthropic + OpenAI-chat wire formats;
  unsupported formats reported and skipped).
- **Three-theme console**, switchable from the sidebar and persisted per browser:
  **Kami** (warm washi-paper, the new default), **Dark**, and **Light** —
  charts, topology graph, and timeline gantt all re-theme.

### Fixed

- **ClickHouse SQL literal escaping** (dialect-aware): a backslash in a
  dimension-filter value could break out of the quoted literal in the ClickHouse
  backend. Escaping is now dialect-aware across both backends.

### Security / CI

- Self-hosted CI runners are gated to same-repo PRs, closing fork-PR code
  execution; a release may only be cut from a commit that passed `staging-soak`,
  and prod deploys are gated on the load soak.

## [0.5.0] — 2026-06-04

### Added — ClickHouse storage backend

- New `h-storage-clickhouse` crate: a drop-in `StorageBackend` implementation
  backed by ClickHouse (HTTP interface, async serde RowBinary via the
  `clickhouse` crate), selected with `storage.backend = "clickhouse"`. Mirrors
  the DuckDB backend's full read + write surface. Fact tables use `MergeTree`;
  `agent_turns` uses `ReplacingMergeTree(_version)` (FINAL reads; version-bumped
  full-row re-insert for `update_turn_metadata`). Timestamps are
  `DateTime64(6, 'UTC')` mapped to `i64` micros. Retention runs via lightweight
  `DELETE` on the shared `[storage.retention]` schedule.
- Backend-neutral logic (dimension-filter SQL builders, header/token-estimate
  converters, the serving-software classifier) extracted from
  `h-storage-duckdb` into `h-storage` (`dialect` / `convert` / `classify`) and
  shared by both backends — single source of truth.
- `storage_bench` binary + `scripts/bench-storage.sh` (`just bench-storage`):
  ClickHouse-vs-DuckDB write-throughput + read-latency comparison through the
  identical workload. Findings + methodology in
  `docs/design/bench-clickhouse-vs-duckdb.md`.
- ClickHouse `query_services` optimised 46× (5129 ms → 112 ms at 1M rows): the
  body-sample `ROW_NUMBER` window (which read every row's ~2 KB body columns)
  became an `id IN (… LIMIT N BY …)` two-phase fetch; `arrayDistinct(groupArray)`
  → `groupUniqArray(N)`; `quantileExact` → `quantileTDigest`.

### Fixed

- **Agent-sessions `agent_kind` multi-select returned nothing.** Selecting more
  than one agent kind sent a CSV (`claude-cli,codex-cli`) that the sessions
  query exact-matched as a single literal, so the list went empty; selecting one
  kind worked. Fixed in both the DuckDB and ClickHouse backends. Root-caused
  beyond the symptom: `agent_kind` is now CSV-parsed once at the API boundary
  into a `Vec` (like every other multi-select filter), so no storage backend
  ever sees a raw CSV to mis-handle.
- **Agent classifier no longer flags unrecognized tool names as "suspicious".**
  Tools from non-Claude-Code agents (e.g. `web_search`, `read_file`,
  `memory_get`) were tagged suspicious purely for being absent from a hardcoded
  registry. Any named tool is now classified as a function-call surface; the
  registry treadmill is gone.

### Internal

- Perf/reliability gate for the release pipeline: a `rate_pps`-throttled
  pcap-file load-soak (`tara --load`) and criterion hot-path micro-benchmarks.
- Deploy pipeline hardening: staging-soak stamps its `staging-soaked` status via
  the REST API (the runner has no `gh`), and `deploy-prod` supersedes older
  waiting approvals instead of letting them pile up and wedge the queue.

## [0.4.0] — 2026-05-29

### Changed — Rebrand to Heron

- Project renamed **TokenScope → Heron**. The binary is now `heron`
  (was `tokenscope`); the 10 internal library crates moved from the
  `ts-*` prefix to `h-*`; the GitHub repo is now `Netis/heron` (the old
  URL redirects). Console title, logo (a new heron mark), and all
  install/docs URLs updated.

### Added — Quality infrastructure

- Deterministic fault-injection harness for the DuckDB backend
  (feature-gated) plus recovery tests that drive the FATAL → reopen →
  every-surface-works path without relying on real load pressure.
- Schema-migration tests over synthesized legacy DB shapes, locking the
  auto-migration paths against silent regression.
- CI lint gates: referenced-secret provisioning, secret-value sanity,
  validated-constructor scoping, and an infra-leakage gate that fails on
  any non-allow-listed private IP or private-key block in tracked files.

### Security / privacy

- Removed the demo deploy tooling, which hard-coded a server address, a
  jump-host username, and a plaintext password. Demo setup is now an
  AI-agent prompt in the docs instead.
- Scrubbed internal infrastructure identity (private IPs, hostnames)
  from source comments, docs, scripts, and test fixtures; tests now use
  RFC5737 documentation ranges.

### Added — Agent-era observer (H002)

- Agent traffic classification: every LlmCall carries `is_agent_request`,
  `tool_surface`, `agent_topology`, `tool_call_count`, `tool_names`. Every
  AgentTurn rolls up `tool_surfaces`, `tool_call_total`, `agent_topology`,
  `suspicious_skills`. New `tool_surface` dimension on `llm_metrics`.
- Console: agent-aware columns and filters on Agent Turns; Agent breakdown
  section on turn detail; tool-surface facet on Performance.
- Config: `[agent_classifier]` block in `default.toml` for tool taxonomy.
- Internal metrics: `agent_classifier.unknown_count`,
  `classifier_mixed_count`.

### Capture

- Default live-capture configuration now covers common LLM-serving ports,
  reducing the need for explicit CLI capture filters in quickstart flows.

### LLM wire-API support

- OpenAI Chat streaming now captures `delta.reasoning_content` and
  `delta.reasoning`, with console rendering before normal content.
- OpenCode agent profile detection added for clients that expose a stable
  `x-session-affinity` anchor.

### Agent turn tracking

- Generic fallback turn grouping now requires a tool/function-call anchor, so
  text-only SDK calls stay on the LLM Calls page instead of producing
  synthetic one-call Agent Turns.

### Metrics

- TTFT handling now distinguishes streaming and non-streaming calls, with
  stream-only TTFT charts and backfilled rollups from stored call data.
- Dashboard active-resource history added for TCP connections and agent
  turns.
- Long-range chart axes use date-aware labels for multi-day windows.

### Console

- Settings page added for capture sources, including interface discovery,
  source editing, grouped source-type controls, and restart flow.
- LLM Calls gained stream/non-stream filtering.
- List pages persist the selected item in the URL.
- Agent-kind filter options are derived from observed data in the active
  window instead of a fixed list.

### API

- `GET /api/capture/interfaces` lists available capture interfaces.
- `PUT /api/capture/sources` updates capture-source configuration and
  restarts the process when needed.

### Documentation

- README reframed around agent observability with refreshed screenshots.
- README quickstart now uses the default live-capture command and no longer
  includes an explicit capture-filter example.
- Removed the LLM call detail screenshot and its README reference.
- Removed project-origin/company copy from public docs.

### Development

- Headless PR review workflow added for CI.
- Repository instructions now require PR text to scrub private environment
  details before publication.

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

Initial release of Heron — an LLM API performance monitoring system that
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
