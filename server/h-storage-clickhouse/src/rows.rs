//! `#[derive(clickhouse::Row)]` structs for INSERT (and full-row SELECT where
//! needed). Field **names** map to ClickHouse columns and field **order**
//! matches the `CREATE TABLE` column order — RowBinary is positional on SELECT
//! and the insert column list is generated from the struct fields, so both must
//! line up with `schema.rs`.
//!
//! Timestamps are `i64` microseconds (the `clickhouse` crate maps `i64`
//! directly to `DateTime64(6)` ticks; `Option<i64>` to `Nullable(DateTime64)`).
//! `From` impls mirror the DuckDB `prepare_*` functions 1:1.

use clickhouse::Row;
use serde::{Deserialize, Serialize};

use h_llm::model::LlmCall;
use h_metrics::model::{LlmFinishMetric, LlmMetric};
use h_protocol::HttpExchange;
use h_turn::Trace;

use h_storage::convert::headers_to_json;

#[derive(Row, Serialize, Deserialize)]
pub(crate) struct CallRow {
    pub id: String,
    pub source_id: String,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
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
    pub cache_read_input_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub ttft_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub response_id: Option<String>,
    pub request_headers: String,
    pub response_headers: String,
    pub is_agent_request: bool,
    pub tool_surface: Option<String>,
    pub agent_topology: Option<String>,
    pub tool_call_count: u32,
    pub tool_names_json: Option<String>,
    pub body_bytes_dropped: u64,
    pub process_pid: Option<u32>,
    pub process_comm: Option<String>,
    pub process_exe: Option<String>,
    /// OTel span kind. Every wire-captured span is an LLM call today; the
    /// column is forward-looking for wire-visible tool spans. Tail field to
    /// match the `spans` table column order (and the DuckDB layout).
    pub kind: String,
}

impl From<LlmCall> for CallRow {
    fn from(c: LlmCall) -> Self {
        CallRow {
            id: c.id,
            source_id: c.source_id,
            client_ip: c.client_ip.to_string(),
            client_port: c.client_port,
            server_ip: c.server_ip.to_string(),
            server_port: c.server_port,
            request_time: c.request_time,
            response_time: c.response_time,
            complete_time: c.complete_time,
            wire_api: c.wire_api.to_string(),
            model: c.model,
            api_type: c.api_type.to_string(),
            is_stream: c.is_stream,
            request_path: c.request_path,
            status_code: c.status_code,
            finish_reason: c.finish_reason,
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            total_tokens: c.total_tokens,
            cache_read_input_tokens: c.cache_read_input_tokens,
            cache_creation_input_tokens: c.cache_creation_input_tokens,
            ttft_ms: c.ttft_ms,
            e2e_latency_ms: c.e2e_latency_ms,
            request_body: c.request_body,
            response_body: c.response_body,
            response_id: c.response_id,
            request_headers: headers_to_json(&c.request_headers),
            response_headers: headers_to_json(&c.response_headers),
            is_agent_request: c.is_agent_request,
            tool_surface: c.tool_surface.map(|s| s.to_string()),
            agent_topology: c.agent_topology.map(|s| s.to_string()),
            tool_call_count: c.tool_call_count,
            tool_names_json: Some(
                serde_json::to_string(&c.tool_names).unwrap_or_else(|_| "[]".to_string()),
            ),
            body_bytes_dropped: c.body_bytes_dropped,
            process_pid: c.process.as_ref().map(|p| p.pid),
            process_comm: c.process.as_ref().map(|p| p.comm.clone()),
            process_exe: c.process.as_ref().and_then(|p| p.exe.clone()),
            kind: "llm".into(),
        }
    }
}

#[derive(Row, Serialize, Deserialize)]
pub(crate) struct MetricRow {
    pub timestamp: i64,
    pub source_id: String,
    pub granularity: String,
    pub wire_api: String,
    pub model: String,
    pub server_ip: String,
    pub call_count: u64,
    pub stream_count: u64,
    pub non_stream_count: u64,
    pub active_calls_sum: u64,
    pub active_calls_sample_count: u64,
    pub active_calls_max: u32,
    pub total_input_tokens: u64,
    pub input_token_count: u64,
    pub total_output_tokens: u64,
    pub output_token_count: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,
    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,
    pub ttft_sum: f64,
    pub ttft_count: u64,
    pub ttft_p50: Option<f64>,
    pub ttft_p95: Option<f64>,
    pub ttft_p99: Option<f64>,
    pub ttft_stream_sum: f64,
    pub ttft_stream_count: u64,
    pub ttft_stream_p50: Option<f64>,
    pub ttft_stream_p95: Option<f64>,
    pub ttft_stream_p99: Option<f64>,
    pub ttft_nonstream_sum: f64,
    pub ttft_nonstream_count: u64,
    pub ttft_nonstream_p50: Option<f64>,
    pub ttft_nonstream_p95: Option<f64>,
    pub ttft_nonstream_p99: Option<f64>,
    pub e2e_sum: f64,
    pub e2e_count: u64,
    pub e2e_p50: Option<f64>,
    pub e2e_p95: Option<f64>,
    pub e2e_p99: Option<f64>,
    pub tpot_sum: f64,
    pub tpot_count: u64,
    pub tpot_p50: Option<f64>,
    pub tpot_p95: Option<f64>,
    pub tpot_p99: Option<f64>,
    pub tool_surface: Option<String>,
}

impl From<LlmMetric> for MetricRow {
    fn from(m: LlmMetric) -> Self {
        MetricRow {
            timestamp: m.timestamp_us,
            source_id: m.source_id,
            granularity: m.granularity.to_string(),
            wire_api: m.wire_api,
            model: m.model,
            server_ip: m.server_ip,
            call_count: m.call_count,
            stream_count: m.stream_count,
            non_stream_count: m.non_stream_count,
            active_calls_sum: m.active_calls_sum,
            active_calls_sample_count: m.active_calls_sample_count,
            active_calls_max: m.active_calls_max,
            total_input_tokens: m.total_input_tokens,
            input_token_count: m.input_token_count,
            total_output_tokens: m.total_output_tokens,
            output_token_count: m.output_token_count,
            total_cache_read_input_tokens: m.total_cache_read_input_tokens,
            total_cache_creation_input_tokens: m.total_cache_creation_input_tokens,
            error_count: m.error_count,
            error_4xx_count: m.error_4xx_count,
            error_429_count: m.error_429_count,
            error_5xx_count: m.error_5xx_count,
            ttft_sum: m.ttft_sum,
            ttft_count: m.ttft_count,
            ttft_p50: m.ttft_p50,
            ttft_p95: m.ttft_p95,
            ttft_p99: m.ttft_p99,
            ttft_stream_sum: m.ttft_stream_sum,
            ttft_stream_count: m.ttft_stream_count,
            ttft_stream_p50: m.ttft_stream_p50,
            ttft_stream_p95: m.ttft_stream_p95,
            ttft_stream_p99: m.ttft_stream_p99,
            ttft_nonstream_sum: m.ttft_nonstream_sum,
            ttft_nonstream_count: m.ttft_nonstream_count,
            ttft_nonstream_p50: m.ttft_nonstream_p50,
            ttft_nonstream_p95: m.ttft_nonstream_p95,
            ttft_nonstream_p99: m.ttft_nonstream_p99,
            e2e_sum: m.e2e_sum,
            e2e_count: m.e2e_count,
            e2e_p50: m.e2e_p50,
            e2e_p95: m.e2e_p95,
            e2e_p99: m.e2e_p99,
            tpot_sum: m.tpot_sum,
            tpot_count: m.tpot_count,
            tpot_p50: m.tpot_p50,
            tpot_p95: m.tpot_p95,
            tpot_p99: m.tpot_p99,
            tool_surface: m.tool_surface,
        }
    }
}

#[derive(Row, Serialize, Deserialize)]
pub(crate) struct FinishMetricRow {
    pub timestamp: i64,
    pub source_id: String,
    pub granularity: String,
    pub wire_api: String,
    pub model: String,
    pub server_ip: String,
    pub finish_reason: String,
    pub count: u64,
}

impl From<LlmFinishMetric> for FinishMetricRow {
    fn from(m: LlmFinishMetric) -> Self {
        FinishMetricRow {
            timestamp: m.timestamp_us,
            source_id: m.source_id,
            granularity: m.granularity,
            wire_api: m.wire_api,
            model: m.model,
            server_ip: m.server_ip,
            finish_reason: m.finish_reason,
            count: m.count,
        }
    }
}

#[derive(Row, Serialize, Deserialize)]
pub(crate) struct TurnRow {
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
    pub models_used: Option<String>,
    pub subagents_used: Option<String>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,
    pub total_cost_usd: Option<f64>,
    pub status: String,
    pub final_finish_reason: Option<String>,
    pub user_input_preview: Option<String>,
    pub user_call_id: Option<String>,
    pub final_answer_preview: Option<String>,
    pub final_call_id: Option<String>,
    // Maps to the `span_ids` column (RowBinary insert names columns by field).
    pub span_ids: String,
    pub metadata: Option<String>,
    pub tool_surfaces_json: Option<String>,
    pub tool_call_total: u32,
    pub agent_topology: Option<String>,
    pub suspicious_skills_json: Option<String>,
    pub _version: u64,
}

impl From<Trace> for TurnRow {
    fn from(t: Trace) -> Self {
        let tool_surfaces_json = {
            let strings: Vec<String> = t.tool_surfaces.iter().map(|s| s.to_string()).collect();
            serde_json::to_string(&strings).unwrap_or_else(|_| "[]".to_string())
        };
        let suspicious_skills_json =
            serde_json::to_string(&t.suspicious_skills).unwrap_or_else(|_| "[]".to_string());
        // Initial finalize version = end_time (micros). `update_trace_metadata`
        // re-inserts with a strictly-greater wall-clock-micros version so the
        // ReplacingMergeTree keeps the latest metadata.
        let version = t.end_time_us.max(0) as u64;
        TurnRow {
            turn_id: t.turn_id,
            source_id: t.source_id,
            session_id: t.session_id,
            wire_api: t.wire_api,
            agent_kind: t.agent_kind,
            client_ip: t.client_ip.to_string(),
            server_ip: t.server_ip.to_string(),
            start_time: t.start_time_us,
            end_time: t.end_time_us,
            duration_ms: t.duration_ms,
            call_count: t.call_count,
            models_used: Some(serde_json::to_string(&t.models_used).unwrap_or_default()),
            subagents_used: Some(serde_json::to_string(&t.subagents_used).unwrap_or_default()),
            total_input_tokens: t.total_input_tokens,
            total_output_tokens: t.total_output_tokens,
            total_cache_read_input_tokens: t.total_cache_read_input_tokens,
            total_cache_creation_input_tokens: t.total_cache_creation_input_tokens,
            total_cost_usd: t.total_cost_usd,
            status: t.status.to_string(),
            final_finish_reason: t.final_finish_reason,
            user_input_preview: t.user_input_preview,
            user_call_id: t.user_call_id,
            final_answer_preview: t.final_answer_preview,
            final_call_id: t.final_call_id,
            span_ids: serde_json::to_string(&t.span_ids).unwrap_or_default(),
            metadata: Some(t.metadata.to_string()),
            tool_surfaces_json: Some(tool_surfaces_json),
            tool_call_total: t.tool_call_total,
            agent_topology: t.agent_topology.map(|top| top.to_string()),
            suspicious_skills_json: Some(suspicious_skills_json),
            _version: version,
        }
    }
}

#[derive(Row, Serialize, Deserialize)]
pub(crate) struct ExchangeRow {
    pub id: String,
    pub source_id: String,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    pub method: String,
    pub uri: String,
    pub request_headers: String,
    pub request_body: Option<String>,
    pub status: Option<u16>,
    pub response_headers: String,
    pub response_body: Option<String>,
    pub is_sse: bool,
    pub sse_event_count: u32,
    pub sse_data_bytes: u64,
    pub request_time: i64,
    pub response_first_byte_time: Option<i64>,
    pub response_complete_time: Option<i64>,
}

impl From<HttpExchange> for ExchangeRow {
    fn from(x: HttpExchange) -> Self {
        let (client_ip, client_port) = x.client_addr();
        let (server_ip, server_port) = x.server_addr();
        let is_sse = x.is_sse();
        // ClickHouse String is byte-safe, but the 0.15 RowBinary validator maps
        // the column to a serde `str`; binary (gzip/protobuf) bodies are stored
        // lossily as UTF-8. LLM HTTP traffic here is post-TLS plaintext
        // JSON/SSE, so this is a rare edge. See module docs / 07-schema.md.
        let response_body = x
            .stored_response_body()
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let request_body = if x.request.body.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&x.request.body).into_owned())
        };
        ExchangeRow {
            id: x.id.clone(),
            source_id: x.request.flow_key.source_id.clone(),
            client_ip: client_ip.to_string(),
            client_port,
            server_ip: server_ip.to_string(),
            server_port,
            method: x.request.method.clone(),
            uri: x.request.uri.clone(),
            request_headers: headers_to_json(&x.request.headers),
            request_body,
            status: Some(x.response.status),
            response_headers: headers_to_json(&x.response.headers),
            response_body,
            is_sse,
            sse_event_count: x.sse_event_count,
            sse_data_bytes: x.sse_data_bytes,
            request_time: x.request.timestamp_us,
            response_first_byte_time: Some(x.response.first_byte_timestamp_us),
            response_complete_time: Some(x.response.complete_timestamp_us),
        }
    }
}
