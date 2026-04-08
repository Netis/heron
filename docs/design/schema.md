# Data Schema Design

## Overview

Three data entities, described in a storage-agnostic format (no SQL DDL). Each entity maps to a table/collection in the chosen storage backend (SQLite / PostgreSQL / ClickHouse).

```
llm_loops  1 ──── N  llm_requests
                          │
                          └──── aggregated into ──── llm_metrics
```

---

## 1. `llm_loops` — Agent Loop Records

One record per agent loop. An agent loop is one complete agent interaction: user ask → multiple LLM calls (think → tool_use → feedback → think → …) → final answer. Generated in the realtime processing pipeline via `LoopTracker`.

```
llm_loops
├── Primary Key
│   └── id: string (UUID v7)
│
├── Association Fields
│   ├── connection_id: string?       # TCP connection identifier (client_ip:port-server_ip:port)
│   ├── tenant_id: string?           # Hashed API key prefix
│   └── client_ip: string
│
├── Time Range
│   ├── start_time: timestamp        # First request arrival time
│   └── end_time: timestamp?         # Last response completion time
│
├── Aggregated Stats (maintained in memory by LoopTracker, written to DB on loop end)
│   ├── request_count: u32
│   ├── total_input_tokens: u64
│   ├── total_output_tokens: u64
│   ├── total_tokens: u64
│   ├── error_count: u32
│   └── tool_use_count: u32          # Number of tool_use rounds
│
├── Model Info
│   ├── provider: string
│   └── model: string
│
└── Status
    └── status: string               # active / completed / timeout / error

Indexes:
  - start_time
  - tenant_id, start_time
  - connection_id
  - status (for finding active loops)
```

### Loop Lifecycle (State Machine)

```
         New request arrives (no active loop for this connection)
                │
                ▼
            ┌────────┐
            │ active │◀──── Request completes with finish_reason = tool_use
            └───┬────┘
                │
    ┌───────────┼───────────────┐
    │           │               │
    ▼           ▼               ▼
completed    timeout         error
(finish !=   (no new         (connection close /
 tool_use)    request         error response)
              within
              threshold)
```

### Loop End Signals by Provider

| Provider | Loop continues | Loop ends |
|---|---|---|
| OpenAI | `finish_reason: "tool_calls"` | `finish_reason: "stop"` or `"length"` |
| Anthropic | `stop_reason: "tool_use"` | `stop_reason: "end_turn"` or `"max_tokens"` |
| Azure | same as OpenAI | same as OpenAI |
| Gemini | function call in response | no function call |
| Generic | usually same as OpenAI | usually same as OpenAI |

These are normalized to `FinishReason::ToolUse` / `FinishReason::Complete` etc. in each `ProviderExtractor`.

---

## 2. `llm_requests` — Per-Request Detail

One record per LLM API call. The core fact table. Includes full request/response body content.

```
llm_requests
├── Primary Key
│   └── id: string (UUID v7, time-ordered)
│
├── Loop Association
│   ├── loop_id: string?             # FK to llm_loops.id (set in realtime pipeline)
│   └── loop_index: u32?             # Sequence within loop (0, 1, 2, ...)
│
├── Association Fields
│   ├── connection_id: string?       # TCP connection identifier
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
│   └── total_tokens: u32?
│
├── Performance Metrics (computed at write time)
│   ├── ttfb_ms: f64?               # Time To First Byte (response_time - request_time)
│   └── e2e_latency_ms: f64?        # End-to-end latency (complete_time - request_time)
│
├── Full Content
│   ├── request_body: string?        # Complete request JSON
│   └── response_body: string?       # Complete response JSON
│
└── Metadata
    └── server_node: string?

Indexes:
  - request_time
  - loop_id, loop_index
  - connection_id, request_time
  - tenant_id, request_time
  - model, request_time
  - status_code, request_time
```

### Design Notes

- **Performance metrics in requests table**: `ttfb_ms` and `e2e_latency_ms` are computed at write time for fast single-record queries. TPOT can be derived: `output_tokens / (complete_time - response_time)`.
- **Full body storage**: `request_body` and `response_body` store complete JSON. For streaming responses, `response_body` contains the concatenated final content.
- **`connection_id`**: Most reliable signal for grouping requests within an agent loop. Format: `{client_ip}:{client_port}-{server_ip}:{server_port}`.

---

## 3. `llm_metrics` — Pre-Aggregated Time-Series

Pre-aggregated metrics by time window + dimension combination. Frontend dashboards query this table exclusively for trend charts and overview panels — never the detail tables.

```
llm_metrics
├── Primary Key (composite)
│   ├── timestamp: timestamp         # Aggregation window start
│   ├── granularity: string          # 10s / 1m / 5m / 1h
│   ├── provider: string             # Dimension value, '*' = all
│   ├── model: string                # '*' = all
│   ├── tenant_id: string            # '*' = all
│   └── server_node: string          # '*' = all
│
├── Request Counts
│   ├── request_count: u64
│   └── error_count: u64             # status_code >= 400
│
├── Token Stats
│   ├── total_input_tokens: u64
│   └── total_output_tokens: u64
│
├── TTFB Distribution
│   ├── ttfb_avg: f64?
│   ├── ttfb_p50: f64?
│   ├── ttfb_p95: f64?
│   └── ttfb_p99: f64?
│
├── TPOT Distribution
│   ├── tpot_avg: f64?
│   ├── tpot_p50: f64?
│   ├── tpot_p95: f64?
│   └── tpot_p99: f64?
│
└── E2E Latency Distribution
    ├── e2e_avg: f64?
    ├── e2e_p50: f64?
    ├── e2e_p95: f64?
    └── e2e_p99: f64?

Indexes:
  - granularity, timestamp
  - granularity, model, timestamp
```

### Design Notes

- **Dimension value `*`**: Represents "all" / unsplit for that dimension. E.g., `model='gpt-4o', tenant_id='*'` = gpt-4o totals across all tenants.
- **Percentiles**: Computed at aggregation time using t-digest or DDSketch approximation, then stored as plain values.
- **Multi-granularity**: Fine-grained (10s) for realtime dashboards, coarse (1h) for historical trends.

---

## Data Lifecycle

Retention periods are configurable. Defaults:

```
llm_requests  →  30 days
llm_loops     →  30 days
llm_metrics
  ├─ 10s      →  1 day
  ├─ 1m       →  7 days
  ├─ 5m       →  30 days
  └─ 1h       →  1 year
```

Expired data is cleaned up by a background task. Each storage backend implements its own cleanup strategy:
- SQLite: periodic DELETE
- PostgreSQL: partition by time + DROP
- ClickHouse: TTL expressions

---

## Storage Backend Adaptation Notes

| Aspect | SQLite | PostgreSQL | ClickHouse |
|---|---|---|---|
| `id` (UUID v7) | TEXT | `uuid` native type | String |
| `timestamp` | TEXT (ISO 8601) | `timestamptz` | `DateTime64(6)` |
| `request_body` / `response_body` | TEXT | TEXT | String |
| `llm_requests` ordering | B-tree on `request_time` | B-tree on `request_time` | `ORDER BY (request_time, id)` MergeTree |
| `llm_loops` ordering | B-tree on `start_time` | B-tree on `start_time` | `ORDER BY (start_time, id)` MergeTree |
| `llm_metrics` optimization | plain table | TimescaleDB hypertable on `timestamp` (optional) | `ORDER BY (granularity, timestamp, model)` |
| Percentile storage | plain f64 | plain f64 | plain f64, or `AggregateFunction(quantilesTDigest, Float64)` for re-aggregation |
| Batch write | WAL mode + batch INSERT | `COPY` | batch INSERT (≥1000 rows per batch) |
| Data expiry | application-level DELETE | `pg_partman` time partition + DROP | TTL expression |
