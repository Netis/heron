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
│   ├── provider: string             # openai / anthropic / azure / gemini / generic
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
├── Provider IDs
│   └── response_id: string?         # Provider's response/message ID (e.g., chatcmpl-xxx, msg_xxx)
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
- **`response_id`**: Provider's response/message ID (e.g., OpenAI `chatcmpl-xxx`, Anthropic `msg_xxx`). Promoted to top-level for fast cross-referencing with provider logs.

---

## 2. `llm_metrics` — Pre-Aggregated Time-Series

Pre-aggregated metrics by time window + dimension combination. Frontend dashboards query this table exclusively for trend charts and overview panels — never the detail tables.

```
llm_metrics
├── Primary Key (composite)
│   ├── timestamp: timestamp         # Aggregation window start
│   ├── stream_id: u32               # Per-source dimension (see below)
│   ├── granularity: string          # 10s / 1m / 5m / 1h
│   ├── provider: string             # Dimension value, '*' = all
│   ├── model: string                # '*' = all
│   └── server_ip: string            # '*' = all
│
├── Traffic
│   ├── request_count: u64
│   ├── stream_count: u64            # Streaming requests
│   ├── non_stream_count: u64
│   ├── concurrency_avg: f64         # Average concurrent requests in window
│   └── concurrency_max: u32         # Peak concurrent requests in window
│
├── Tokens
│   ├── total_input_tokens: u64
│   ├── total_output_tokens: u64
│   ├── input_tokens_avg: f64?       # Avg input tokens per request
│   ├── output_tokens_avg: f64?      # Avg output tokens per request
│   ├── total_cache_read_input_tokens: u64    # Sum of cache_read_input_tokens
│   └── total_cache_creation_input_tokens: u64 # Sum of cache_creation_input_tokens
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
├── TTFB Distribution
│   ├── ttfb_avg: f64?
│   ├── ttfb_p50: f64?
│   ├── ttfb_p95: f64?
│   └── ttfb_p99: f64?
│
├── E2E Latency Distribution
│   ├── e2e_avg: f64?
│   ├── e2e_p50: f64?
│   ├── e2e_p95: f64?
│   └── e2e_p99: f64?
│
├── TPOT Distribution (streaming only, ms/token)
│   ├── tpot_avg: f64?               # Avg ms per output token
│   ├── tpot_p50: f64?
│   ├── tpot_p95: f64?
│   └── tpot_p99: f64?

Indexes:
  - granularity, timestamp
  - granularity, model, timestamp
```

### Design Notes

- **`stream_id`**: Per-capture-source dimension so each stream keeps an independent event-time watermark — without it, clock skew between sources (cloud-probe vs. local pcap) would re-open already-flushed windows and emit duplicate rows. Rows that share `(timestamp, granularity, provider, model, server_ip)` but differ by `stream_id` are merged at query time via `GROUP BY timestamp` + SUM for counts and weighted average (weighted by `request_count`, or `stream_count` for TPOT) for averages/percentiles. Today `stream_id` equals the 0-based index of the capture source in `[[capture.sources]]`; the pipeline-to-stream mapping may decouple later (e.g. fan-out or merged streams), so API/frontend treat `stream_id` as internal and never filter on it.
- **Aggregation levels**: finest `(provider, model, server_ip)` for drilldown, global `(*, *, *)` for overview. Additional dimensions (tenant_id, etc.) will be added as they are validated with real traffic.
- **Other dimension analysis**: query `llm_calls` detail table with GROUP BY for dimensions not yet in pre-aggregation.
- **Percentiles**: Computed at aggregation time using t-digest or DDSketch approximation, then stored as plain values.
- **Multi-granularity**: Fine-grained (10s) for realtime dashboards, coarse (1h) for historical trends.
- **Concurrency**: Measured by maintaining a counter (+1 on request arrival, -1 on completion), sampled per second within the window.
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
