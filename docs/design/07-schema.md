# Data Schema Design

## Overview

Four data entities, described in a storage-agnostic format (no SQL DDL). Each entity maps to a table/collection in the chosen storage backend (DuckDB / PostgreSQL / ClickHouse).

```
traces  ─── 1:N ───  spans  ─── aggregated into ───  llm_metrics
                                                         ╲
                                                          ─── llm_finish_metrics
```

Per the project-wide read-path rule, cross-entity reads never use `JOIN` — `traces` carries its child `span_ids` inline and detail reads issue a follow-up `IN (?, ?, ...)` lookup against `spans`. Finish-reason counts live in their own long-format table (`llm_finish_metrics`) so the wide pre-aggregation table can keep a fixed column set.

---

## 1. `spans` — Per-Request Detail

One record per LLM API call. The core fact table. Includes full request/response body content.

```
spans
├── Primary Key
│   └── id: string (UUID v7, time-ordered)
│
├── Association Fields
│   ├── client_ip: string
│   ├── client_port: u16
│   └── server_port: u16
│
├── Timestamps
│   ├── request_time: timestamp      # Request arrival time
│   ├── response_time: timestamp?    # First response byte time
│   └── complete_time: timestamp?    # Response completion time
│
├── Request Info
│   ├── wire_api: string             # openai-chat / openai-responses / anthropic / ...
│   ├── model: string
│   ├── api_type: string             # chat / embedding / image / completion
│   ├── is_stream: bool
│   └── request_path: string
│
├── Response Info
│   ├── status_code: u16?
│   └── finish_reason: string?       # raw provider value, verbatim (see "finish_reason vocabulary" below)
│
├── Token Stats
│   ├── input_tokens: u32?
│   ├── output_tokens: u32?
│   ├── total_tokens: u32?
│   ├── cache_read_input_tokens: u32?   # Anthropic cache_read / OpenAI cached_tokens
│   └── cache_creation_input_tokens: u32? # Anthropic cache_creation; None for OpenAI
│
├── Performance Metrics (computed at write time)
│   ├── ttft_ms: f64?               # Time To First Token (response_time - request_time)
│   └── e2e_latency_ms: f64?        # End-to-end latency (complete_time - request_time)
│
├── Wire-API IDs
│   └── response_id: string?         # Wire API's response/message ID (e.g., chatcmpl-xxx, msg_xxx)
│
├── Full Content
│   ├── request_body: string?        # Complete request JSON
│   ├── response_body: string?       # Complete response JSON
│   ├── request_headers: string?     # JSON array of [key, value] pairs
│   └── response_headers: string?    # JSON array of [key, value] pairs
│
└── Metadata
    └── server_ip: string

Indexes:
  - request_time
  - model, request_time
  - status_code, request_time
```

### Design Notes

- **Performance metrics in requests table**: `ttft_ms` and `e2e_latency_ms` are computed at write time for fast single-record queries. Per-call Token Throughput can be derived: `output_tokens / (complete_time - response_time)` (tokens/s).
- **Full body storage**: `request_body` and `response_body` store complete JSON. For streaming responses, `response_body` contains the concatenated final content.
- **Headers storage**: `request_headers` and `response_headers` store complete HTTP headers as JSON arrays of `[key, value]` pairs, preserving order and allowing duplicate keys. Rate limit info, request IDs, processing time, etc. can be queried from stored headers without top-level extraction.
- **`response_id`**: Wire API's response/message ID (e.g., OpenAI `chatcmpl-xxx`, Anthropic `msg_xxx`). Promoted to top-level for fast cross-referencing with vendor logs.

### `finish_reason` vocabulary

Raw provider value, verbatim. The owning row's `wire_api` determines which vocabulary applies:

| `wire_api` | Possible values |
|---|---|
| `anthropic` | `end_turn`, `stop_sequence`, `max_tokens`, `tool_use`, `pause_turn`, `refusal`, `model_context_window_exceeded` |
| `openai-chat` | `stop`, `length`, `tool_calls`, `function_call`, `content_filter` |
| `openai-responses` | `completed`, `incomplete`, `failed`, `cancelled` |

Future provider values flow through verbatim; no normalization is performed. The `WireApi::is_terminal(&str)` and `WireApi::is_tool_use(&str)` predicates encode wire-level semantics and are the canonical way to interpret these values in code — consumers (tracker, metrics, UI) MUST NOT hardcode comparisons against the strings above.

> **Migration note.** Rows written before the raw-string refactor (see `CHANGELOG`) carry the legacy normalized labels (`complete`, `length`, `tool_use`, `error`, `cancelled`). No reverse migration is performed; queries that mix old and new data must handle both. New rows always carry raw provider values, distinguishable by row date.

---

## 2. `traces` — One Agent Interaction (1:N over `spans`)

One record per agent turn (a contiguous sequence of `spans` for a single agent run). Inline `span_ids` keeps cross-entity reads JOIN-free per the project-wide rule.

```
traces
├── Primary Key
│   └── id: string (UUID v7, time-ordered, minted at finalize)
│
├── Identity & Association
│   ├── agent_kind: string            # e.g. anthropic / openai-codex / generic
│   ├── session_id: string?           # client-supplied session marker, when available
│   ├── source_id: u32                # capture source (matches llm_metrics.source_id)
│   └── span_ids: string[]            # ordered list of spans.id in this turn
│
├── Timestamps
│   ├── start_time: timestamp         # first call's request_time
│   └── end_time: timestamp           # last call's complete_time (or finalize-time if incomplete)
│
├── Outcome
│   ├── status: string                # complete | incomplete (see below)
│   └── final_finish_reason: string?  # raw finish_reason of the call that closed the turn
│
└── Text
    ├── user_input_preview: string?
    └── final_answer_preview: string?

Indexes:
  - end_time
  - agent_kind, end_time
```

### Field notes

- **`status`** (`VARCHAR NOT NULL`) — Two values only: `complete` (a wire-level terminal landed before finalize) or `incomplete` (idle timeout, pcap EOF, server shutdown, or connection RST mid-stream). The wire-level reason — `end_turn`, `max_tokens`, `refusal`, etc. — lives in `final_finish_reason`. Older databases written when the column carried `length` / `failed` / `cancelled` are upgraded in place on init: `length → complete`, `failed`/`cancelled → incomplete`. The wire reason for those legacy rows is unrecoverable.
- **`final_finish_reason`** (`VARCHAR`, nullable) — Raw `finish_reason` of the call that closed this turn. Same vocabulary as `spans.finish_reason` (per `wire_api`); same migration caveat for rows pre-dating the raw-string refactor.
- **`span_ids`** carries the ordered child UUIDs inline so detail reads can issue a single `SELECT ... FROM spans WHERE id IN (?, ?, ...)` follow-up. See `query_trace_spans` in `h-storage-duckdb/src/turns.rs` for the canonical pattern. Storing this list inline is the project's chosen alternative to a relational JOIN.

---

## 3. `llm_metrics` — Pre-Aggregated Time-Series

Pre-aggregated metrics by time window + dimension combination. Frontend dashboards query this table exclusively for trend charts and overview panels — never the detail tables.

**Multi-row per key.** The aggregator drains each bucket on a fixed per-granularity cadence (see `05-metrics.md`). A fast response emits **one** row per `(timestamp, source_id, granularity, wire_api, model, server_ip)` key; a slow response whose Complete arrives after its bucket was already drained opens a fresh bucket for the same window and emits an **additional** row at the next cadence. The key is therefore a row-identity marker, **not** a unique primary key — queries always use `GROUP BY timestamp [+ dim]` with `SUM()` to collapse multiple slices into the window total. Every average-style field is stored as a `(sum, count)` pair to make this collapse exact.

```
llm_metrics
├── Row Key (composite; NOT unique — see note above)
│   ├── timestamp: timestamp         # Aggregation window start (from request_time)
│   ├── source_id: u32               # Per-source dimension (see below)
│   ├── granularity: string          # 10s / 1m / 5m / 1h
│   ├── wire_api: string             # Dimension value, '*' = all
│   ├── model: string                # '*' = all
│   └── server_ip: string            # '*' = all
│
├── Traffic
│   ├── call_count: u64
│   ├── stream_count: u64            # Streaming requests
│   ├── non_stream_count: u64
│   ├── active_calls_sum: u64         # Σ active-calls samples
│   ├── active_calls_sample_count: u64
│   └── active_calls_max: u32         # Peak active calls in row's slice
│
├── Tokens
│   ├── total_input_tokens: u64
│   ├── input_token_count: u64       # Pair with total_input_tokens for avg
│   ├── total_output_tokens: u64
│   ├── output_token_count: u64
│   ├── total_cache_read_input_tokens: u64
│   └── total_cache_creation_input_tokens: u64
│
├── Errors
│   ├── error_count: u64             # All errors (status_code >= 400)
│   ├── error_4xx_count: u64         # Client errors (400-499)
│   ├── error_429_count: u64         # Rate limiting (ops focus)
│   └── error_5xx_count: u64         # Server errors (500-599)
│
│   # NOTE: per-finish_reason counts now live in `llm_finish_metrics` (long-format,
│   # see section 4). The five legacy `finish_*_count` columns were dropped because
│   # the raw-string refactor made the value space unbounded and future-extensible.
│
├── TTFT Distribution (milliseconds)
│   ├── ttft_sum: f64                # Σ TTFT samples (exact)
│   ├── ttft_count: u64              # # TTFT samples (exact)
│   ├── ttft_p50: f64?               # Per-row t-digest estimate over this slice
│   ├── ttft_p95: f64?
│   └── ttft_p99: f64?
│
├── E2E Latency Distribution (milliseconds)
│   ├── e2e_sum: f64
│   ├── e2e_count: u64
│   ├── e2e_p50: f64?
│   ├── e2e_p95: f64?
│   └── e2e_p99: f64?
│
├── TPOT Distribution (streaming only, ms/token)
│   ├── tpot_sum: f64
│   ├── tpot_count: u64
│   ├── tpot_p50: f64?
│   ├── tpot_p95: f64?
│   └── tpot_p99: f64?

Indexes:
  - granularity, timestamp
  - granularity, model, timestamp
```

### Design Notes

- **`source_id`**: Per-capture-source dimension so each source keeps an independent event-time watermark — without it, clock skew between sources (cloud-probe vs. local pcap) would re-open already-flushed windows and emit duplicate rows. Today `source_id` equals the 0-based index of the capture source in `[[capture.sources]]`; the pipeline-to-source mapping may decouple later (e.g. fan-out or merged sources), so API/frontend treat `source_id` as internal and never filter on it.
- **Query-time aggregation.** Rows that share `(timestamp, granularity, wire_api, model, server_ip)` — whether they differ by `source_id` or are multiple drain slices of the same source — are merged by `GROUP BY timestamp [+ dim]`:
  - Plain counters / totals → `SUM()`.
  - Averages → `SUM(*_sum) / SUM(*_count)` (exact).
  - `active_calls_max` → `MAX()`.
  - Percentiles → `SUM(*_p* * *_count) / SUM(*_count)` (approximation — weighting by the matching `*_count` keeps slow-response rows with `call_count=0` from collapsing the result to zero, but it is not equivalent to merging the underlying t-digests. Serialized t-digest bytes is the planned long-term fix.)
- **Aggregation levels**: finest `(wire_api, model, server_ip)` for drilldown, global `(*, *, *)` for overview. Additional dimensions will be added as they are validated with real traffic.
- **Other dimension analysis**: query `spans` detail table with GROUP BY for dimensions not yet in pre-aggregation.
- **`*_sum / *_count` instead of `*_avg`**: averages are not additive across rows; storing the exact sum and count lets the query layer SUM over any set of rows (multi-source, multi-drain-slice) and divide to get a correct average. The per-row percentiles (`*_p*`) are t-digest estimates over that row's slice only — single-row views can read them directly.
- **Multi-granularity**: Fine-grained (10s) for realtime dashboards, coarse (1h) for historical trends. Each granularity has its own drain cadence equal to its window size, so steady-state row count per granularity matches the number of windows covered.
- **Active Calls**: Per-`DimensionKey` counter (+1 on `Start`, -1 on `Complete`); every Start writes the current value as a sample into `active_calls_sum / active_calls_sample_count` and updates `active_calls_max`. Cross-row avg via the `sum / count` pair; peak via `MAX(active_calls_max)`.
- **Derivable metrics** (computed at query time, not stored): Call Rate (`call_count / window_seconds`), Call Success Rate (`1 - error_count / call_count`), Token Throughput in tokens/s (`total_output_tokens / window_seconds`), Cache Hit Ratio (`total_cache_read_input_tokens / total_input_tokens`).

---

## 4. `llm_finish_metrics` — Long-Format Finish-Reason Counts

Per-bucket counts of `finish_reason` occurrences. Long-format because the value space is unbounded (raw provider values, future-extensible per provider). Splitting this out of `llm_metrics` keeps the wide table's column set fixed while letting finish-reason cardinality grow.

```
llm_finish_metrics
├── Row Key (composite; same multi-row caveat as llm_metrics)
│   ├── timestamp: timestamp         # Bucket start
│   ├── source_id: string            # Capture source ('*' for rolled-up tier)
│   ├── granularity: string          # 10s / 1m / 5m / 1h
│   ├── wire_api: string             # '*' = all
│   ├── model: string                # '*' = all
│   ├── server_ip: string            # always '*' (no per-server tier)
│   └── finish_reason: string        # raw provider value
│
└── Counter
    └── count: u64                   # calls in this bucket with this finish_reason

Indexes:
  - timestamp, granularity
```

### Design Notes

- **No JOIN on read.** Readers query `llm_finish_metrics` directly via the `/api/metrics/finish-reasons` endpoint; nothing in the read path joins it back to `llm_metrics`. Splitting the table does not introduce a JOIN — it only narrows each row.
- **Dimension tiers.** The aggregator emits up to four tiers per bucket: `(W, M, S)`, `(W, M, *)`, `(*, *, S)`, `(*, *, *)`. Reads pick the matching tier based on the request's filter shape; `wire_api` / `model` filters with multiple values use SQL `IN` to OR within a tier.
- **Vocabulary.** `finish_reason` carries the same raw provider values listed in section 1. The `wire_api` column distinguishes which vocabulary the value belongs to; for the `(*, *, *)` rolled-up tier the `wire_api` column is `*` and the underlying values may be from any provider — readers that need per-provider breakdowns must request a `(W, ...)` tier explicitly.
- **Multi-row per key.** Same drain-cadence semantics as `llm_metrics`: late completes can produce additional rows for an already-drained window. Queries always use `GROUP BY ... finish_reason` with `SUM(count)` to collapse.

---

## Data Lifecycle

Retention is **enabled by default** with sane TTLs (`calls = turns = 30` days, `http_exchanges = 7` days). Operators tune via `[storage.retention]`; set `enabled = false` to opt out, or set any field to `0` to make that table never expire. A background sweeper (spawned at startup, cancelled on Ctrl+C) runs every `check_interval_secs` (default 3600) and deletes rows older than the per-table / per-granularity cutoff.

**Cutoff columns** (what "old" means):
- `spans.request_time`
- `traces.end_time` (NOT NULL; turn completion — safer than start_time)
- `llm_metrics.timestamp`, further keyed by `granularity`
- `llm_finish_metrics.timestamp`, further keyed by `granularity` (sweeper reuses the `llm_metrics` per-granularity cutoffs)

**Cross-table constraint:** `traces` must not outlive `spans`. `traces.span_ids` references `spans.id`, and the no-JOIN trace-detail read trusts those references — traces surviving past their spans would render with empty/partial span lists. `validate()` enforces `traces <= spans` (with `spans = 0` treated as infinite, satisfying any finite `traces`). The pre-rename keys `calls`/`turns` are still accepted as deprecated serde aliases.

**Defaults** (active out of the box):

```toml
[storage.retention]
enabled = true
check_interval_secs = 3600
spans = 30    # per-call detail (spans) max age in days; caps `traces`
traces = 30   # agent-trace summaries max age; must be <= spans (or set spans = 0)
http_exchanges = 7

[storage.retention.metrics]
"10s" = 1
"1m"  = 7
"5m"  = 30
"1h"  = 365
```

Each backend implements `StorageBackend::apply_retention` with a dialect-appropriate strategy:
- **DuckDB** (current): per-table DELETE + `CHECKPOINT` once per sweep to reclaim on-disk space (DuckDB DELETEs are MVCC tombstones until checkpoint).
- **PostgreSQL** (planned): simple DELETE, or declarative partitioning + `DROP TABLE partition`; with TimescaleDB, `drop_chunks`.
- **ClickHouse** (implemented): per-table lightweight `DELETE FROM ... WHERE <col> < cutoff` (counts gathered via a `count()` immediately before each delete, since CH `DELETE` returns no affected-row count); `traces`/`llm_metrics` use the same per-table / per-granularity cutoffs as DuckDB. Optional `OPTIMIZE TABLE ... FINAL` per swept table when `optimize_on_sweep = true` (off by default — background merges reclaim lazily). No declarative `TTL` is set, so a single `apply_retention` path drives both backends identically.

---

## Storage Backend Adaptation Notes

| Aspect | DuckDB | PostgreSQL | ClickHouse |
|---|---|---|---|
| `id` (UUID v7) | VARCHAR | `uuid` native type | String |
| `timestamp` | TIMESTAMP | `timestamptz` | `DateTime64(6)` |
| `request_body` / `response_body` | VARCHAR or JSON | TEXT | String |
| `spans` ordering | B-tree on `request_time` | B-tree on `request_time` | `ORDER BY (request_time, id)` MergeTree |
| `llm_metrics` optimization | plain table | TimescaleDB hypertable on `timestamp` (optional) | `ORDER BY (granularity, timestamp, model)` |
| `llm_finish_metrics` optimization | plain table | TimescaleDB hypertable on `timestamp` (optional) | `ORDER BY (granularity, timestamp, finish_reason)` |
| Percentile storage | plain DOUBLE | plain f64 | plain f64, or `AggregateFunction(quantilesTDigest, Float64)` for re-aggregation |
| Batch write | batch INSERT (appender API) | `COPY` | batch INSERT (≥1000 rows per batch) |
| Data expiry | periodic DELETE | `pg_partman` time partition + DROP | TTL expression |

---

## Upgrade Notes

### OpenTelemetry-aligned rename (`agent_turns`→`traces`, `llm_calls`→`spans`)

Aligns the storage + API vocabulary with the industry-standard Session → Trace →
Span hierarchy (a session is the existing `(source_id, session_id)` view over
traces).

- Tables `agent_turns` → `traces`, `llm_calls` → `spans`; column
  `traces.call_ids` → `span_ids`.
- New forward-looking `spans.kind` column (always `'llm'` today) so future
  wire-visible tool spans can carry `kind='tool'`.
- **In-place auto-migration** on init() (unlike the older rename below): an
  idempotent detect-then-rename runs before `CREATE TABLE IF NOT EXISTS`, so
  existing DuckDB/ClickHouse databases migrate without data loss — no need to
  delete the file. `RENAME` has no `IF EXISTS`, so each is guarded by a
  table/column existence check.
- Back-compat: HTTP routes `/api/agent-turns*` and `/api/llm-calls*` keep
  working as deprecated aliases (RFC 8594 `Deprecation` header) for the
  canonical `/api/traces*` and `/api/spans*`. Retention config keys
  `calls`/`turns` remain accepted as serde aliases for `spans`/`traces`.

### `AgentTurn` rename (`LlmTurn` → `AgentTurn`)

- Table `llm_turns` → `agent_turns` (now further renamed to `traces` — see above)
- Column `client_kind` → `agent_kind`

No online migration was performed for this older rename. Existing
`server/data/heron.duckdb` files from before *this* rename should be deleted
before restart — the backend recreates the schema on first run via
`CREATE TABLE IF NOT EXISTS`.

### `finish_reason` raw-string refactor (see `CHANGELOG`)

Idempotent on-init migrations on the DuckDB backend:

- `llm_metrics`: `ALTER TABLE ... DROP COLUMN IF EXISTS finish_complete_count` (and the four sibling columns: `finish_length_count`, `finish_tool_use_count`, `finish_error_count`, `finish_cancelled_count`).
- `traces`: one-time `UPDATE traces SET status='complete' WHERE status='length'` and `UPDATE traces SET status='incomplete' WHERE status IN ('failed','cancelled')`. After this update the legacy `length` / `failed` / `cancelled` values cannot reappear; the wire reason for those rows is unrecoverable.
- `spans.finish_reason` is **not** rewritten. Pre-refactor rows keep their normalized labels (`complete`, `length`, `tool_use`, `error`, `cancelled`); post-refactor rows carry raw provider values. The two are distinguishable by row date. Application code that filters or groups across the boundary must handle both vocabularies.
- `llm_finish_metrics` is created via `CREATE TABLE IF NOT EXISTS` on first run; no historical backfill from the old `finish_*_count` columns is performed.
