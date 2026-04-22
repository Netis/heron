use serde::Serialize;

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub start_us: i64,
    pub end_us: i64,
}

#[derive(Debug, Clone, Default)]
pub struct DimensionFilter {
    pub wire_apis: Vec<String>,
    pub models: Vec<String>,
    pub server_ips: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MetricsTimeseriesQuery {
    pub time_range: TimeRange,
    pub granularity: String,
    pub filter: DimensionFilter,
    pub fields: Vec<String>,
    pub group_by: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MetricsSummaryQuery {
    pub time_range: TimeRange,
    pub filter: DimensionFilter,
}

#[derive(Debug, Clone)]
pub struct MetricsModelsQuery {
    pub time_range: TimeRange,
    pub filter: DimensionFilter,
    pub sort_by: String,
    pub sort_order: String,
    pub limit: u32,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Serialize)]
pub struct MetricsTimeseriesRow {
    pub timestamp: i64,
    pub group: Option<String>,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSummaryRow {
    pub request_count: u64,
    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub ttfb_avg: Option<f64>,
    pub e2e_avg: Option<f64>,
    pub tpot_avg: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsModelRow {
    pub wire_api: String,
    pub model: String,
    pub request_count: u64,
    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub ttfb_avg: Option<f64>,
    pub ttfb_p95: Option<f64>,
    pub e2e_avg: Option<f64>,
    pub e2e_p95: Option<f64>,
    pub tpot_avg: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallListItem {
    pub id: String,
    pub stream_id: String,
    pub request_time: i64,
    pub wire_api: String,
    pub model: String,
    pub status_code: Option<u16>,
    pub is_stream: bool,
    pub finish_reason: Option<String>,
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallsPage {
    pub total: u64,
    pub items: Vec<CallListItem>,
}

#[derive(Debug, Clone)]
pub struct HttpExchangesQuery {
    pub time_range: TimeRange,
    /// Server IPs to filter by. Empty = no filter. Matches
    /// `DimensionFilter.server_ips` for the Requests page.
    pub server_ips: Vec<String>,
    /// Uppercase HTTP method strings (GET, POST, …). Empty = no filter.
    pub methods: Vec<String>,
    /// HTTP status codes. Empty = no filter. Exchanges with `status IS NULL`
    /// are excluded when this filter is non-empty.
    pub status_codes: Vec<u16>,
    /// `Some(true)` → SSE only. `Some(false)` → non-SSE only. `None` → any.
    pub is_sse: Option<bool>,
    /// One of `"request_time"`, `"status"`, `"duration_ms"`. Validated server-side.
    pub sort_by: String,
    /// `"asc"` or `"desc"`.
    pub sort_order: String,
    pub page: u32,
    pub page_size: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpExchangeListItem {
    pub id: String,
    pub stream_id: String,
    /// µs since epoch.
    pub request_time: i64,
    pub method: String,
    pub uri: String,
    pub client_ip: String,
    pub server_ip: String,
    pub server_port: u16,
    pub status: Option<u16>,
    pub is_sse: bool,
    /// `complete_time - request_time` in milliseconds, or `None` when the
    /// exchange is incomplete (no response yet / will never arrive).
    pub duration_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpExchangesPage {
    pub total: u64,
    pub items: Vec<HttpExchangeListItem>,
}

#[derive(Debug, Clone)]
pub struct TurnsQuery {
    pub time_range: TimeRange,
    pub filter: DimensionFilter,
    pub statuses: Vec<String>,
    pub agent_kinds: Vec<String>,
    pub sort_by: String,
    pub sort_order: String,
    pub page: u32,
    pub page_size: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnListItem {
    pub turn_id: String,
    pub stream_id: String,
    pub session_id: String,
    pub start_time: i64,
    pub end_time: i64,
    pub duration_ms: u64,
    pub wire_api: String,
    pub agent_kind: String,
    pub primary_model: Option<String>,
    pub models_used: Vec<String>,
    pub call_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub status: String,
    pub final_finish_reason: Option<String>,
    pub user_input_preview: Option<String>,
    pub final_answer_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnsPage {
    pub total: u64,
    pub items: Vec<TurnListItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnDetail {
    pub turn_id: String,
    pub stream_id: String,
    pub session_id: String,
    pub tenant_id: Option<String>,
    pub wire_api: String,
    pub agent_kind: String,
    pub start_time: i64,
    pub end_time: i64,
    pub duration_ms: u64,
    pub call_count: u32,
    pub models_used: Vec<String>,
    pub subagents_used: Vec<String>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,
    pub total_cost_usd: Option<f64>,
    pub status: String,
    pub final_finish_reason: Option<String>,
    pub user_call_id: Option<String>,
    pub user_input: Option<String>,
    pub final_call_id: Option<String>,
    pub final_answer: Option<String>,
    pub call_ids: Vec<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnCallItem {
    pub id: String,
    pub sequence: u32,
    pub request_time: i64,
    pub response_time: Option<i64>,
    pub complete_time: Option<i64>,
    pub wire_api: String,
    pub model: String,
    pub status_code: Option<u16>,
    pub is_stream: bool,
    pub finish_reason: Option<String>,
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    /// Only populated by `query_turn_calls` (for in-API-layer parsing).
    /// Not serialized to the API response — the route strips it after parsing.
    #[serde(skip)]
    pub request_body: Option<String>,
    #[serde(skip)]
    pub response_body: Option<String>,
}

/// Detail view of an `http_exchanges` row — used by `GET /api/http-exchanges/:id`
/// and by the `?include=http` enrichment on `GET /api/llm-calls/:id` (future).
#[derive(Debug, Clone, Serialize)]
pub struct HttpExchangeDetail {
    pub id: String,
    pub stream_id: String,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    pub method: String,
    pub uri: String,
    /// JSON-encoded array of `[header_name, header_value]` pairs (same shape
    /// as `llm_calls.request_headers`).
    pub request_headers: String,
    /// Raw request body as a UTF-8 string. May be empty for GET/HEAD.
    pub request_body: Option<String>,
    pub status: Option<u16>,
    pub response_headers: String,
    /// Raw response body as a UTF-8 string. `None` for SSE (body wasn't
    /// retained) or incomplete exchanges.
    pub response_body: Option<String>,
    pub is_sse: bool,
    /// Microseconds since Unix epoch.
    pub request_time: i64,
    pub response_first_byte_time: Option<i64>,
    pub response_complete_time: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallDetail {
    pub id: String,
    pub stream_id: String,
    pub request_time: i64,
    pub response_time: Option<i64>,
    pub complete_time: Option<i64>,
    pub wire_api: String,
    pub model: String,
    pub api_type: String,
    pub is_stream: bool,
    pub request_path: String,
    pub status_code: Option<u16>,
    pub finish_reason: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub response_id: Option<String>,
    pub tenant_id: Option<String>,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub request_headers: Option<String>,
    pub response_headers: Option<String>,
    /// Request body of the immediate successor call in the same turn, if any.
    /// Used by Phase-3 tool-result join in the API layer.
    #[serde(skip)]
    pub next_call_request_body: Option<String>,
}
