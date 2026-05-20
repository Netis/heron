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
pub struct ServicesQuery {
    pub time_range: TimeRange,
    pub sort_by: String,
    pub sort_order: String,
    pub limit: u32,
}

/// One row of the "Services" view: a unique `(server_ip, server_port)`
/// endpoint with the models it served, error/perf stats, and the time
/// window where it appeared. Computed directly off `llm_calls` because
/// the pre-aggregated `llm_metrics` table doesn't carry `server_port`
/// (its grouping sets stop at `server_ip`).
#[derive(Debug, Clone, Serialize)]
pub struct ServiceRow {
    pub server_ip: String,
    pub server_port: u16,
    /// Distinct models seen on this endpoint. Capped at 32 in SQL via
    /// `list_distinct(... )[:32]` so a misbehaving client that sends
    /// thousands of made-up model strings doesn't bloat a single row.
    pub models: Vec<String>,
    pub wire_apis: Vec<String>,
    /// Distinct request paths seen at this endpoint. Used by the
    /// classifier to spot Ollama (`/api/chat`) and llama.cpp
    /// (`/completion`, `/tokenize`) from their native non-OpenAI
    /// surface. Capped at 8 in SQL.
    pub request_paths: Vec<String>,
    pub call_count: u64,
    pub error_count: u64,
    pub stream_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub ttft_avg_ms: Option<f64>,
    pub ttft_p95_ms: Option<f64>,
    pub e2e_avg_ms: Option<f64>,
    pub e2e_p95_ms: Option<f64>,
    /// Unix-epoch milliseconds of the first / last call seen in the
    /// query window. Useful for "is this endpoint still live?".
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    /// Best-effort serving-software identification — one of
    /// `vllm`, `sglang`, `ollama`, `llamacpp`, `litellm`,
    /// `openai-compat`, `openai`, `anthropic`, or `None` (unknown).
    /// vLLM / SGLang both run under uvicorn and can't yet be told
    /// apart from headers alone; both show up as `openai-compat`
    /// today. See `apps::classify_app`.
    pub app: Option<String>,
    /// Raw `Server` HTTP response header — surfaced in the UI as a
    /// tooltip so the user can override the classifier visually.
    pub server_header: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CallsQuery {
    pub time_range: TimeRange,
    pub filter: DimensionFilter,
    pub status_codes: Vec<u16>,
    pub finish_reasons: Vec<String>,
    pub client_ips: Vec<String>,
    pub request_path_contains: Option<String>,
    /// Optional stream-mode filter. `None` keeps everything; `Some(true)` /
    /// `Some(false)` narrows to streaming-only / non-streaming-only.
    pub is_stream: Option<bool>,
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

/// One series of per-bucket counts for a single raw `finish_reason` value, as
/// served by `GET /api/metrics/finish-reasons`. Read directly from
/// `llm_finish_metrics`; no normalization is applied.
#[derive(Debug, Clone, Serialize)]
pub struct FinishReasonTimeseries {
    pub finish_reason: String,
    /// `(timestamp_us, count)` pairs, ordered by timestamp ascending.
    pub points: Vec<(i64, u64)>,
}

/// Query for `GET /api/metrics/finish-reasons`. `wire_apis` / `models` /
/// `server_ips` filter to specific dimensions when non-empty (matches any value
/// in the list, like `MetricsTimeseriesQuery`); empty rolls up across all values
/// via the pre-aggregated `*` dimension tier in `llm_finish_metrics`.
#[derive(Debug, Clone)]
pub struct FinishReasonsQuery {
    pub time_range: TimeRange,
    pub granularity: String,
    pub wire_apis: Vec<String>,
    pub models: Vec<String>,
    pub server_ips: Vec<String>,
}

/// One distinct `(wire_api, finish_reason)` pair observed in the
/// `llm_finish_metrics` table. Served by `GET /api/filters/finish-reasons` and
/// used by the calls-page filter dropdown to populate its options dynamically.
#[derive(Debug, Clone, Serialize)]
pub struct DistinctFinishReason {
    pub wire_api: String,
    pub finish_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSummaryRow {
    pub call_count: u64,
    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub ttft_avg: Option<f64>,
    pub e2e_avg: Option<f64>,
    pub tpot_avg: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsModelRow {
    pub wire_api: String,
    pub model: String,
    pub call_count: u64,
    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub ttft_avg: Option<f64>,
    pub ttft_p95: Option<f64>,
    pub e2e_avg: Option<f64>,
    pub e2e_p95: Option<f64>,
    pub tpot_avg: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallListItem {
    pub id: String,
    pub source_id: String,
    pub request_time: i64,
    pub wire_api: String,
    pub model: String,
    pub status_code: Option<u16>,
    pub is_stream: bool,
    pub finish_reason: Option<String>,
    pub ttft_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    /// True when the row's tokens came from the fallback tiktoken estimator
    /// rather than a wire-side `usage` block. Computed at read time from
    /// `response_body`; not a stored column.
    #[serde(default)]
    pub tokens_estimated: bool,
    pub client_ip: String,
    pub server_ip: String,
    pub server_port: u16,
    pub request_path: String,
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
    /// Client IPs to filter by. Empty = no filter.
    pub client_ips: Vec<String>,
    /// Uppercase HTTP method strings (GET, POST, …). Empty = no filter.
    pub methods: Vec<String>,
    /// HTTP status codes. Empty = no filter. Exchanges with `status IS NULL`
    /// are excluded when this filter is non-empty.
    pub status_codes: Vec<u16>,
    /// Substring (case-sensitive) to match against `uri` via `LIKE '%…%'`.
    pub uri_contains: Option<String>,
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
    pub source_id: String,
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
    /// Per-call client IP filter. `DimensionFilter` only carries `server_ips`
    /// (the metrics-pre-aggregated dimension); client IP lives outside the
    /// filter, parallel to `CallsQuery.client_ips`.
    pub client_ips: Vec<String>,
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
    pub source_id: String,
    pub session_id: String,
    pub start_time: i64,
    pub end_time: i64,
    pub duration_ms: u64,
    pub wire_api: String,
    pub agent_kind: String,
    pub client_ip: String,
    pub server_ip: String,
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

/// One turn row returned by the session-turns endpoint. Identical to
/// `TurnListItem` except `user_input_preview` / `final_answer_preview` are
/// replaced by full-text `user_input` / `final_answer` (server-side
/// reconstructed from the referenced call bodies, see
/// `query_session_turns` in `duckdb.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct SessionTurnItem {
    pub turn_id: String,
    pub source_id: String,
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
    pub user_input: Option<String>,
    pub final_answer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTurnsPage {
    pub items: Vec<SessionTurnItem>,
    /// Opaque cursor for the next page. `None` when the current page is the
    /// last one (fewer than `page_size` rows were returned).
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnDetail {
    pub turn_id: String,
    pub source_id: String,
    pub session_id: String,
    pub wire_api: String,
    pub agent_kind: String,
    pub client_ip: String,
    pub server_ip: String,
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
    pub ttft_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    /// True when the row's tokens came from the fallback estimator. Mirrors
    /// `CallListItem.tokens_estimated`.
    #[serde(default)]
    pub tokens_estimated: bool,
    pub request_path: String,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    /// Raw request body. Frontend parses per-wire_api for preview + detail.
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    /// JSON-encoded `[[header_name, header_value], ...]`. Frontend uses for
    /// the Raw HTTP drawer — no extra fetch needed.
    pub request_headers: Option<String>,
    pub response_headers: Option<String>,
}

/// Detail view of an `http_exchanges` row — used by `GET /api/http-exchanges/:id`
/// and by the `?include=http` enrichment on `GET /api/llm-calls/:id` (future).
#[derive(Debug, Clone, Serialize)]
pub struct HttpExchangeDetail {
    pub id: String,
    pub source_id: String,
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
    /// Number of SSE events observed. `0` for non-SSE exchanges.
    pub sse_event_count: u32,
    /// Sum of SSE `data:` payload bytes. Frame overhead excluded; raw SSE
    /// wire bytes are not retained. `0` for non-SSE exchanges.
    pub sse_data_bytes: u64,
    /// Microseconds since Unix epoch.
    pub request_time: i64,
    pub response_first_byte_time: Option<i64>,
    pub response_complete_time: Option<i64>,
}

/// Cursor for paginating the session list, sorted by last-turn-in-window DESC.
///
/// Tuple order matches the SQL `HAVING (MAX(end_time), source_id, session_id) < (?,?,?)`
/// comparison used on the server side. Encoded/decoded as a base64 JSON blob at
/// the API boundary; internally we carry the raw tuple.
#[derive(Debug, Clone)]
pub struct SessionListCursor {
    /// Matches the `last_turn_at_in_window` of the previous page's last row,
    /// in the same unit as `SessionListItem` timestamps (milliseconds since epoch).
    pub last_turn_at_ms: i64,
    pub source_id: String,
    pub session_id: String,
}

/// List query for `agent_sessions` (view over `agent_turns`).
///
/// Semantics: a session is **included** when at least one of its turns has
/// `end_time` inside `time_range` (turn-in-window inclusion). Aggregated
/// counters / timestamps on each returned row cover the **entire lifetime** of
/// the session, not just the window — see `SessionListItem` docs.
#[derive(Debug, Clone)]
pub struct SessionListQuery {
    pub time_range: TimeRange,
    /// Optional source filter. `None` = all sources. Same-session turns share
    /// a `source_id` (TurnTracker partition key), so pushing this into the
    /// WHERE clause is safe and does not truncate lifetime aggregates.
    pub source_id: Option<String>,
    /// Optional agent_kind filter. Also session-stable, so WHERE is safe.
    pub agent_kind: Option<String>,
    pub cursor: Option<SessionListCursor>,
    pub page_size: u32,
}

/// One row of the session list. Aggregates span the session's **full**
/// history (not just `time_range`); `last_turn_at_in_window` is what the
/// cursor and ORDER BY actually key off.
#[derive(Debug, Clone, Serialize)]
pub struct SessionListItem {
    pub source_id: String,
    pub session_id: String,
    pub agent_kind: String,
    /// ms since epoch. MAX(end_time) across **windowed** turns — the sort key.
    pub last_turn_at_in_window: i64,
    /// ms since epoch. MIN(start_time) across **all** turns of the session.
    pub first_turn_at: i64,
    /// ms since epoch. MAX(end_time) across **all** turns of the session.
    pub last_turn_at: i64,
    pub turn_count: u64,
    pub call_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,
    pub total_cost_usd: Option<f64>,
    /// `user_input_preview` of the earliest turn (min start_time). Captures
    /// the opening topic of the session.
    pub first_user_input_preview: Option<String>,
    /// `user_call_id` of that earliest turn — FE can fetch the full body.
    pub first_user_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionsPage {
    pub items: Vec<SessionListItem>,
    /// Opaque cursor for the next page. `None` when fewer than `page_size`
    /// rows were returned (i.e. the current page is the last one).
    pub next_cursor: Option<String>,
}

/// Detail view for a single session. Identical field set to `SessionListItem`
/// minus the window-dependent `last_turn_at_in_window` — the detail page
/// has no time window.
#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    pub source_id: String,
    pub session_id: String,
    pub agent_kind: String,
    pub first_turn_at: i64,
    pub last_turn_at: i64,
    pub turn_count: u64,
    pub call_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,
    pub total_cost_usd: Option<f64>,
    pub first_user_input_preview: Option<String>,
    pub first_user_call_id: Option<String>,
}

/// Hex-encoded JSON blob. Opaque to the client; `decode_session_cursor` is the
/// only supported reader. Hex keeps us URL-safe without pulling in base64.
pub fn encode_session_cursor(c: &SessionListCursor) -> String {
    let json = serde_json::json!({
        "t": c.last_turn_at_ms,
        "s": c.source_id,
        "k": c.session_id,
    })
    .to_string();
    let mut out = String::with_capacity(json.len() * 2);
    for b in json.as_bytes() {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

pub fn decode_session_cursor(s: &str) -> Option<SessionListCursor> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_digit(bytes[i])?;
        let lo = hex_digit(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    let v: serde_json::Value = serde_json::from_slice(&out).ok()?;
    Some(SessionListCursor {
        last_turn_at_ms: v.get("t")?.as_i64()?,
        source_id: v.get("s")?.as_str()?.to_string(),
        session_id: v.get("k")?.as_str()?.to_string(),
    })
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Cursor for paginating a session's turns (most-recent first).
///
/// Tuple order matches `ORDER BY start_time DESC, turn_id DESC` on the server
/// side, so comparison `(start_time, turn_id) < (?, ?)` steps through pages
/// without duplicates even when two turns share a microsecond.
#[derive(Debug, Clone)]
pub struct SessionTurnsCursor {
    pub start_time_us: i64,
    pub turn_id: String,
}

pub fn encode_session_turns_cursor(c: &SessionTurnsCursor) -> String {
    let json = serde_json::json!({ "t": c.start_time_us, "k": c.turn_id }).to_string();
    let mut out = String::with_capacity(json.len() * 2);
    for b in json.as_bytes() {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

pub fn decode_session_turns_cursor(s: &str) -> Option<SessionTurnsCursor> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_digit(bytes[i])?;
        let lo = hex_digit(bytes[i + 1])?;
        decoded.push((hi << 4) | lo);
    }
    let v: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let start_time_us = v.get("t")?.as_i64()?;
    let turn_id = v.get("k")?.as_str()?.to_string();
    Some(SessionTurnsCursor {
        start_time_us,
        turn_id,
    })
}

#[derive(Debug, Clone)]
pub struct SessionTurnsQuery {
    pub source_id: String,
    pub session_id: String,
    pub cursor: Option<SessionTurnsCursor>,
    pub page_size: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallDetail {
    pub id: String,
    pub source_id: String,
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
    /// True when the row's tokens came from the fallback estimator. See
    /// `CallListItem.tokens_estimated`.
    #[serde(default)]
    pub tokens_estimated: bool,
    pub ttft_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub response_id: Option<String>,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub request_headers: Option<String>,
    pub response_headers: Option<String>,
}

#[cfg(test)]
mod session_turns_cursor_tests {
    use super::*;

    #[test]
    fn session_turns_cursor_roundtrip() {
        let c = SessionTurnsCursor {
            start_time_us: 1_729_000_000_000_000,
            turn_id: "abc-123".to_string(),
        };
        let encoded = encode_session_turns_cursor(&c);
        let decoded = decode_session_turns_cursor(&encoded).expect("decode");
        assert_eq!(decoded.start_time_us, c.start_time_us);
        assert_eq!(decoded.turn_id, c.turn_id);
    }

    #[test]
    fn session_turns_cursor_rejects_garbage() {
        assert!(decode_session_turns_cursor("abc").is_none()); // odd length
        assert!(decode_session_turns_cursor("not-hex!").is_none());
        assert!(decode_session_turns_cursor("00").is_none()); // valid hex, invalid JSON
    }
}
