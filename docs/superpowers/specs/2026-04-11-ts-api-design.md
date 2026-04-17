# ts-api Design Spec

## Overview

REST API module for TokenScope frontend. Serves pre-aggregated metrics from `llm_metrics` and per-request detail from `llm_calls`. Built with Axum, runs as a `tokio::spawn` task alongside the capture pipeline.

**Scope (v1):** Pages 1-6 of the frontend design (Overview, Performance, Traffic, Errors, Models, Requests). Turns page (Page 7) deferred until turn tracking is implemented.

## Unified Response Structure

All endpoints return the same JSON envelope:

```json
// Success
{
  "code": 0,
  "message": "ok",
  "data": { ... }
}

// Error
{
  "code": 1001,
  "message": "invalid parameter: granularity must be one of 10s, 1m, 5m, 1h",
  "data": {}
}
```

- On error, `data` is always `{}` (empty object), never `null`.
- HTTP status codes remain RESTful (200/400/404/500); `code` provides business-level granularity.

### Error Codes

| Range | Meaning |
|-------|---------|
| 0 | Success |
| 1xxx | Parameter errors (1001 invalid param, 1002 missing required param) |
| 2xxx | Data errors (2001 record not found) |
| 5xxx | Internal errors (5001 storage query failed) |

### Rust Implementation

- `ApiResponse<T: Serialize>` struct implementing Axum's `IntoResponse`.
- `ApiError` enum implementing `IntoResponse`, auto-mapping to HTTP status + error code.

## Common Query Parameters

All metrics endpoints (`/api/metrics/*`) share these parameters, corresponding to the frontend Global Toolbar:

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `start` | i64 | yes | Start time, Unix seconds |
| `end` | i64 | yes | End time, Unix seconds |
| `granularity` | string | yes | `10s` / `1m` / `5m` / `1h` |
| `provider` | string | no | Comma-separated filter, e.g. `openai,anthropic` |
| `model` | string | no | Comma-separated filter |
| `server_ip` | string | no | Comma-separated filter |

- Frontend converts "Last 5m / 15m / 1h..." shortcuts to absolute `start/end` timestamps. API only accepts absolute times.
- Frontend handles "Auto" granularity selection. API does not infer granularity.
- When dimension filters are empty, query uses `provider='*' AND model='*' AND server_ip='*'` (global aggregate rows). When specific values are provided, query matches those dimension rows.
- API layer converts seconds to microseconds internally for DB queries.

## API Endpoints

### `GET /api/metrics/timeseries`

Universal time-series data source for all chart types.

**Additional parameters:**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `fields` | string | yes | Comma-separated metric fields to return, e.g. `ttfb_p50,ttfb_p95,e2e_p50,e2e_p95` |
| `group_by` | string | no | Group by dimension: `provider` or `model` |

**Response data:**

```json
{
  "timestamps": [1700000000, 1700000010, 1700000020],
  "series": [
    {
      "name": "ttfb_p50",
      "group": null,
      "values": [120.0, 115.0, 130.0]
    },
    {
      "name": "ttfb_p95",
      "group": null,
      "values": [350.0, 340.0, 360.0]
    }
  ]
}
```

With `group_by=provider`:

```json
{
  "timestamps": [1700000000, 1700000010],
  "series": [
    { "name": "request_count", "group": "openai", "values": [42, 38] },
    { "name": "request_count", "group": "anthropic", "values": [15, 20] }
  ]
}
```

**Frontend chart mapping:**

| Page | Chart | fields | group_by |
|------|-------|--------|----------|
| Overview | Request Volume | `request_count` | `provider` |
| Overview | Latency Overview | `ttfb_p50,ttfb_p95,e2e_p50,e2e_p95` | - |
| Performance | TTFB Distribution | `ttfb_p50,ttfb_p95,ttfb_p99` | - |
| Performance | E2E Distribution | `e2e_p50,e2e_p95,e2e_p99` | - |
| Performance | Output Throughput | `throughput_p50,throughput_p95` | - |
| Performance | Concurrency | `concurrency_avg,concurrency_max` | - |
| Performance | Input Tokens Distribution | `input_tokens_p50,input_tokens_p95,input_tokens_p99` | - |
| Performance | Output Tokens Distribution | `output_tokens_p50,output_tokens_p95,output_tokens_p99` | - |
| Traffic | Request Volume | `request_count` | `provider` |
| Traffic | Token Usage | `total_input_tokens,total_output_tokens` | - |
| Traffic | Finish Reason Breakdown | `finish_complete_count,finish_length_count,finish_tool_use_count,finish_error_count,finish_cancelled_count` | - |
| Traffic | Token Distribution | `input_tokens_p50,input_tokens_p95,output_tokens_p50,output_tokens_p95` | - |
| Errors | Error Timeline | `error_4xx_count,error_429_count,error_5xx_count` | - |
| Errors | Error by Model | `error_count,error_4xx_count,error_429_count,error_5xx_count` | `model` |
| Errors | 429 Trend | `error_429_count` | - |
| Models | Latency Over Time (selected) | `ttfb_p50,ttfb_p95,e2e_p50,e2e_p95` | - |
| Models | Volume & Error Rate (selected) | `request_count,error_count` | - |

### `GET /api/metrics/summary`

KPI summary for Overview and Errors page cards.

**Parameters:** Common params only (start/end + dimension filters). No `granularity` needed.

**Backend logic:** Queries global aggregate rows (`provider='*', model='*', server_ip='*'`) from `llm_metrics` using the finest granularity that covers the requested time range (prefer `10s`, fall back to `1m` etc. based on data availability). Sums count fields (`request_count`, `error_count`, etc.) and computes weighted averages for distribution fields (weighted by `request_count`).

**Response data:**

```json
{
  "request_count": 12500,
  "error_count": 230,
  "error_4xx_count": 180,
  "error_429_count": 45,
  "error_5xx_count": 50,
  "total_input_tokens": 5000000,
  "total_output_tokens": 2500000,
  "ttfb_avg": 145.0,
  "e2e_avg": 1200.0,
  "throughput_avg": 42.0
}
```

### `GET /api/metrics/models`

Per-model aggregated table data for Overview (Model Breakdown), Traffic (Top Models Table), and Models page.

**Additional parameters:**

| Parameter | Type | Required | Default |
|-----------|------|----------|---------|
| `sort_by` | string | no | `request_count` |
| `sort_order` | string | no | `desc` |
| `limit` | u32 | no | 20 |

**Response data:**

```json
{
  "models": [
    {
      "provider": "openai",
      "model": "gpt-4",
      "request_count": 5000,
      "error_count": 50,
      "error_4xx_count": 30,
      "error_429_count": 10,
      "error_5xx_count": 20,
      "total_input_tokens": 2000000,
      "total_output_tokens": 1000000,
      "ttfb_avg": 130.0,
      "ttfb_p95": 350.0,
      "e2e_avg": 1100.0,
      "e2e_p95": 2500.0,
      "throughput_avg": 45.0
    }
  ]
}
```

### `GET /api/calls`

Request list with lightweight fields and pagination.

**Parameters:**

| Parameter | Type | Required | Default |
|-----------|------|----------|---------|
| `start` | i64 | yes | - |
| `end` | i64 | yes | - |
| `provider` | string | no | - |
| `model` | string | no | - |
| `server_ip` | string | no | - |
| `status_code` | string | no | Comma-separated, e.g. `429,500` |
| `finish_reason` | string | no | Comma-separated, e.g. `complete,length` |
| `sort_by` | string | no | `request_time` |
| `sort_order` | string | no | `desc` |
| `page` | u32 | no | 1 |
| `page_size` | u32 | no | 50 (max 200) |

**Response data:**

```json
{
  "total": 12500,
  "items": [
    {
      "id": "01912345-6789-7abc-def0-123456789abc",
      "request_time": 1700000000,
      "provider": "openai",
      "model": "gpt-4",
      "status_code": 200,
      "is_stream": true,
      "finish_reason": "complete",
      "ttfb_ms": 120.0,
      "e2e_latency_ms": 1000.0,
      "input_tokens": 100,
      "output_tokens": 50
    }
  ]
}
```

### `GET /api/calls/{id}`

Full detail for a single request, including body and headers.

**Response data:** All fields from `llm_calls` table, including `request_body`, `response_body`, `request_headers`, `response_headers`, plus all fields from the list view.

### `GET /api/filters/{dimension}`

Three endpoints returning available dimension values for dropdown filters:

```
GET /api/filters/providers   -> { "values": ["openai", "anthropic"] }
GET /api/filters/models      -> { "values": ["gpt-4", "claude-3"] }
GET /api/filters/server_ips  -> { "values": ["10.0.0.1", "10.0.0.2"] }
```

Queries `SELECT DISTINCT` from `llm_metrics`, excluding `*` wildcard rows.

## StorageBackend Trait Extension

Extend the existing `StorageBackend` trait in ts-storage with query methods. Query parameter types and return types are defined in ts-storage to keep the trait independent of HTTP concerns.

### Query Parameter Types

```rust
pub struct TimeRange {
    pub start_us: i64,
    pub end_us: i64,
}

pub struct DimensionFilter {
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub server_ips: Vec<String>,
}

pub struct MetricsTimeseriesQuery {
    pub time_range: TimeRange,
    pub granularity: String,
    pub filter: DimensionFilter,
    pub fields: Vec<String>,
    pub group_by: Option<String>,
}

pub struct MetricsSummaryQuery {
    pub time_range: TimeRange,
    pub filter: DimensionFilter,
}

pub struct MetricsModelsQuery {
    pub time_range: TimeRange,
    pub filter: DimensionFilter,
    pub sort_by: String,
    pub sort_order: String,
    pub limit: u32,
}

pub struct CallsQuery {
    pub time_range: TimeRange,
    pub filter: DimensionFilter,
    pub status_codes: Vec<u16>,
    pub finish_reasons: Vec<String>,
    pub sort_by: String,
    pub sort_order: String,
    pub page: u32,
    pub page_size: u32,
}
```

### Extended Trait

```rust
#[async_trait]
pub trait StorageBackend: Send + Sync {
    // Existing write methods
    async fn init(&self) -> Result<()>;
    async fn write_calls(&self, calls: &[LlmCall]) -> Result<()>;
    async fn write_metrics(&self, metrics: &[LlmMetric]) -> Result<()>;

    // Query methods
    async fn query_metrics_timeseries(&self, query: &MetricsTimeseriesQuery) -> Result<Vec<MetricsTimeseriesRow>>;
    async fn query_metrics_summary(&self, query: &MetricsSummaryQuery) -> Result<MetricsSummaryRow>;
    async fn query_metrics_models(&self, query: &MetricsModelsQuery) -> Result<Vec<MetricsModelRow>>;
    async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage>;
    async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>>;
    async fn query_distinct_providers(&self) -> Result<Vec<String>>;
    async fn query_distinct_models(&self) -> Result<Vec<String>>;
    async fn query_distinct_server_ips(&self) -> Result<Vec<String>>;
}
```

Return types (`MetricsTimeseriesRow`, `MetricsSummaryRow`, `MetricsModelRow`, `CallsPage`, `CallDetail`) are defined in ts-storage. ts-api converts them into API response structures.

## Crate Structure

```
server/ts-api/
├── Cargo.toml
└── src/
    ├── lib.rs           # Public start_server function
    ├── response.rs      # ApiResponse<T>, ApiError
    ├── params.rs        # HTTP query param structs -> storage query type conversion
    └── routes/
        ├── mod.rs       # Router assembly
        ├── metrics.rs   # /api/metrics/* handlers
        ├── calls.rs     # /api/calls, /api/calls/{id} handlers
        └── filters.rs   # /api/filters/* handlers
```

### Dependencies

```toml
[dependencies]
axum = "0.8"
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
ts-common = { workspace = true }
ts-storage = { workspace = true }
tower-http = { version = "0.6", features = ["cors"] }
```

## Server Startup

ts-api exposes a single public function:

```rust
pub async fn start_server(
    config: &ApiConfig,
    storage: Arc<dyn StorageBackend>,
) -> Result<()>
```

Called from `main.rs` via `tokio::spawn`, running alongside the capture pipeline:

```rust
let api_storage = storage.clone();
let api_config = config.api.clone();
tokio::spawn(async move {
    if let Err(e) = ts_api::start_server(&api_config, api_storage).await {
        tracing::error!("API server error: {e}");
    }
});
```

## Shared State and Concurrency

The API and pipeline share the same `Arc<dyn StorageBackend>` instance. For DuckDB, the underlying `Arc<Mutex<Connection>>` serializes reads and writes. This is acceptable for single-node deployments.

CORS is configured with `CorsLayer::permissive()` for development (frontend Vite dev server on `:5173` accessing backend on `:8080`).

No connection pooling in v1. Future optimization paths:
- DuckDB: multiple connections with read/write separation
- PostgreSQL/ClickHouse: native connection pools
