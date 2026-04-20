# Data Schema Design

## Overview

Three data entities, described in a storage-agnostic format (no SQL DDL). Each entity maps to a table/collection in the chosen storage backend (DuckDB / PostgreSQL / ClickHouse).

```
llm_calls  ──── aggregated into ──── llm_metrics
```

---

## 1. `llm_calls` — Per-Request Detail

One record per LLM API call. The core fact table. Includes full request/response body content.

```
llm_calls
├── Primary Key
│   └── id: string (UUID v7, time-ordered)
│
├── Association Fields
│   ├── tenant_id: string?           # Hashed API key prefix
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
│   └── finish_reason: string?       # complete / length / error / cancelled / tool_use (normalized)
│
├── Token Stats
│   ├── input_tokens: u32?
│   ├── output_tokens: u32?
│   ├── total_tokens: u32?
│   ├── cache_read_input_tokens: u32?   # Anthropic cache_read / OpenAI cached_tokens
│   └── cache_creation_input_tokens: u32? # Anthropic cache_creation; None for OpenAI
│
├── Performance Metrics (computed at write time)
│   ├── ttfb_ms: f64?               # Time To First Byte (response_time - request_time)
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
  - tenant_id, request_time
  - model, request_time
  - status_code, request_time
```

### Design Notes

- **Performance metrics in requests table**: `ttfb_ms` and `e2e_latency_ms` are computed at write time for fast single-record queries. Per-request throughput can be derived: `output_tokens / (complete_time - response_time)` (tokens/s).
- **Full body storage**: `request_body` and `response_body` store complete JSON. For streaming responses, `response_body` contains the concatenated final content.
- **Headers storage**: `request_headers` and `response_headers` store complete HTTP headers as JSON arrays of `[key, value]` pairs, preserving order and allowing duplicate keys. Rate limit info, request IDs, processing time, etc. can be queried from stored headers without top-level extraction.
- **`response_id`**: Wire API's response/message ID (e.g., OpenAI `chatcmpl-xxx`, Anthropic `msg_xxx`). Promoted to top-level for fast cross-referencing with vendor logs.

---

## 2. `llm_metrics` — Pre-Aggregated Time-Series

Pre-aggregated metrics by time window + dimension combination. Frontend dashboards query this table exclusively for trend charts and overview panels — never the detail tables.

**Multi-row per key.** The aggregator drains each bucket on a fixed per-granularity cadence (see `05-metrics.md`). A fast response emits **one** row per `(timestamp, stream_id, granularity, wire_api, model, server_ip)` key; a slow response whose Complete arrives after its bucket was already drained opens a fresh bucket for the same window and emits an **additional** row at the next cadence. The key is therefore a row-identity marker, **not** a unique primary key — queries always use `GROUP BY timestamp [+ dim]` with `SUM()` to collapse multiple slices into the window total. Every average-style field is stored as a `(sum, count)` pair to make this collapse exact.

```
llm_metrics
├── Row Key (composite; NOT unique — see note above)
│   ├── timestamp: timestamp         # Aggregation window start (from request_time)
│   ├── stream_id: u32               # Per-source dimension (see below)
│   ├── granularity: string          # 10s / 1m / 5m / 1h
│   ├── wire_api: string             # Dimension value, '*' = all
│   ├── model: string                # '*' = all
│   └── server_ip: string            # '*' = all
│
├── Traffic
│   ├── request_count: u64
│   ├── stream_count: u64            # Streaming requests
│   ├── non_stream_count: u64
│   ├── concurrency_sum: u64         # Σ per-call concurrency samples
│   ├── concurrency_sample_count: u64
│   └── concurrency_max: u32         # Peak concurrent requests in row's slice
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
├── Finish Reason Counts
│   ├── finish_complete_count: u64   # Normal completion
│   ├── finish_length_count: u64     # Truncated by max_tokens
│   ├── finish_tool_use_count: u64   # Tool/function call (agent pattern)
│   ├── finish_error_count: u64      # Generation error
│   └── finish_cancelled_count: u64  # Client cancelled
│
├── TTFB Distribution (milliseconds)
│   ├── ttfb_sum: f64                # Σ TTFB samples (exact)
│   ├── ttfb_count: u64              # # TTFB samples (exact)
│   ├── ttfb_p50: f64?               # Per-row t-digest estimate over this slice
│   ├── ttfb_p95: f64?
│   └── ttfb_p99: f64?
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

- **`stream_id`**: Per-capture-source dimension so each stream keeps an independent event-time watermark — without it, clock skew between sources (cloud-probe vs. local pcap) would re-open already-flushed windows and emit duplicate rows. Today `stream_id` equals the 0-based index of the capture source in `[[capture.sources]]`; the pipeline-to-stream mapping may decouple later (e.g. fan-out or merged streams), so API/frontend treat `stream_id` as internal and never filter on it.
- **Query-time aggregation.** Rows that share `(timestamp, granularity, wire_api, model, server_ip)` — whether they differ by `stream_id` or are multiple drain slices of the same stream — are merged by `GROUP BY timestamp [+ dim]`:
  - Plain counters / totals → `SUM()`.
  - Averages → `SUM(*_sum) / SUM(*_count)` (exact).
  - `concurrency_max` → `MAX()`.
  - Percentiles → `SUM(*_p* * *_count) / SUM(*_count)` (approximation — weighting by the matching `*_count` keeps slow-response rows with `request_count=0` from collapsing the result to zero, but it is not equivalent to merging the underlying t-digests. Serialized t-digest bytes is the planned long-term fix.)
- **Aggregation levels**: finest `(wire_api, model, server_ip)` for drilldown, global `(*, *, *)` for overview. Additional dimensions (tenant_id, etc.) will be added as they are validated with real traffic.
- **Other dimension analysis**: query `llm_calls` detail table with GROUP BY for dimensions not yet in pre-aggregation.
- **`*_sum / *_count` instead of `*_avg`**: averages are not additive across rows; storing the exact sum and count lets the query layer SUM over any set of rows (multi-stream, multi-drain-slice) and divide to get a correct average. The per-row percentiles (`*_p*`) are t-digest estimates over that row's slice only — single-row views can read them directly.
- **Multi-granularity**: Fine-grained (10s) for realtime dashboards, coarse (1h) for historical trends. Each granularity has its own drain cadence equal to its window size, so steady-state row count per granularity matches the number of windows covered.
- **Concurrency**: Per-`DimensionKey` counter (+1 on `Start`, -1 on `Complete`); every Start writes the current value as a sample. Cross-row avg via the `sum / count` pair; peak via `MAX(concurrency_max)`.
- **Derivable metrics** (computed at query time, not stored): QPS (`request_count / window_seconds`), success rate (`1 - error_count / request_count`), aggregate throughput in tokens/s (`total_output_tokens / window_seconds`), cache hit ratio (`total_cache_read_input_tokens / total_input_tokens`).

---

## Data Lifecycle

Retention is **disabled by default**; operators opt in via `[storage.retention]` in config. Once enabled, a background sweeper (spawned at startup, cancelled on Ctrl+C) runs every `check_interval_secs` (default 3600) and deletes rows older than the per-table / per-granularity cutoff. A value of `0` (or a field absent) means "never expire" for that table/granularity.

**Cutoff columns** (what "old" means):
- `llm_calls.request_time`
- `llm_turns.end_time` (NOT NULL; turn completion — safer than start_time)
- `llm_metrics.timestamp`, further keyed by `granularity`

**Recommended defaults** (set explicitly in config; no built-in defaults to avoid surprise deletion):

```toml
[storage.retention]
enabled = true
check_interval_secs = 3600
calls = 7     # llm_calls max age in days
turns = 30    # llm_turns max age in days

[storage.retention.metrics]
"10s" = 1
"1m"  = 7
"5m"  = 30
"1h"  = 365
```

Each backend implements `StorageBackend::apply_retention` with a dialect-appropriate strategy:
- **DuckDB** (current): per-table DELETE + `CHECKPOINT` once per sweep to reclaim on-disk space (DuckDB DELETEs are MVCC tombstones until checkpoint).
- **PostgreSQL** (planned): simple DELETE, or declarative partitioning + `DROP TABLE partition`; with TimescaleDB, `drop_chunks`.
- **ClickHouse** (planned): declarative `TTL ... INTERVAL N DAY` on the MergeTree at init; `apply_retention` degrades to `OPTIMIZE TABLE ... FINAL` or a no-op.

---

## Storage Backend Adaptation Notes

| Aspect | DuckDB | PostgreSQL | ClickHouse |
|---|---|---|---|
| `id` (UUID v7) | VARCHAR | `uuid` native type | String |
| `timestamp` | TIMESTAMP | `timestamptz` | `DateTime64(6)` |
| `request_body` / `response_body` | VARCHAR or JSON | TEXT | String |
| `llm_calls` ordering | B-tree on `request_time` | B-tree on `request_time` | `ORDER BY (request_time, id)` MergeTree |
| `llm_metrics` optimization | plain table | TimescaleDB hypertable on `timestamp` (optional) | `ORDER BY (granularity, timestamp, model)` |
| Percentile storage | plain DOUBLE | plain f64 | plain f64, or `AggregateFunction(quantilesTDigest, Float64)` for re-aggregation |
| Batch write | batch INSERT (appender API) | `COPY` | batch INSERT (≥1000 rows per batch) |
| Data expiry | periodic DELETE | `pg_partman` time partition + DROP | TTL expression |
