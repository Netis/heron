use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

/// The type of LLM API call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiType {
    Chat,
    // Embedding, Image, Completion — future
}

impl fmt::Display for ApiType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiType::Chat => write!(f, "chat"),
        }
    }
}

/// Normalized finish reason across providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// Normal completion (OpenAI: "stop", Anthropic: "end_turn")
    Complete,
    /// Max tokens reached
    Length,
    /// Tool use — agent trace continues
    ToolUse,
    /// Error during generation
    Error,
    /// Request was cancelled
    Cancelled,
}

impl fmt::Display for FinishReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FinishReason::Complete => write!(f, "complete"),
            FinishReason::Length => write!(f, "length"),
            FinishReason::ToolUse => write!(f, "tool_use"),
            FinishReason::Error => write!(f, "error"),
            FinishReason::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A fully extracted LLM API call record.
#[derive(Debug, Clone)]
pub struct LlmCall {
    pub stream_id: String,
    pub id: String,
    /// Stable wire-API identifier (e.g. "anthropic", "openai-chat",
    /// "openai-responses"). Sourced from `WireApi::name()`; persisted verbatim
    /// to storage. This is the HTTP API shape, not the vendor.
    pub wire_api: &'static str,
    pub model: String,
    pub api_type: ApiType,
    pub tenant_id: Option<String>,
    pub request_time: i64,
    pub response_time: Option<i64>,
    pub complete_time: Option<i64>,
    pub request_path: String,
    pub is_stream: bool,
    pub request_body: Option<String>,
    pub status_code: Option<u16>,
    pub finish_reason: Option<FinishReason>,
    pub response_body: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub client_ip: IpAddr,
    pub client_port: u16,
    pub server_ip: IpAddr,
    pub server_port: u16,
    /// Provider's response/message ID for cross-referencing with provider logs.
    pub response_id: Option<String>,
    /// Raw HTTP request headers. Serialization format is decided by each storage backend.
    pub request_headers: Vec<(String, String)>,
    /// Raw HTTP response headers. Serialization format is decided by each storage backend.
    pub response_headers: Vec<(String, String)>,
}

/// Identity of an LLM call once an `AgentProfile` has matched — i.e. the call
/// is attributed to a known agent client (claude-cli, codex-cli, …). Non-agent
/// traffic never produces an `AgentIdentity`.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    /// Short stable agent name (e.g. `"claude-cli"`). Doubles as the profile
    /// selector (look up via `AgentProfileRegistry::find_by_name`) and the
    /// persisted `AgentTurn.agent_kind` storage value.
    pub agent_kind: &'static str,
    pub session_id: String,
    /// Explicit turn_id from request body when the profile provides one (e.g. Codex).
    /// `None` when the turn shard will generate the turn_id (Anthropic path).
    pub turn_id_hint: Option<String>,
}

/// An LlmCall attributed to a specific agent. The `call` is an `Arc` because
/// the same `LlmCall` is fanned out to the storage sink and the turn shard in
/// parallel — all consumers read-only, no mutex needed.
#[derive(Debug, Clone)]
pub struct AgentCall {
    pub call: Arc<LlmCall>,
    pub agent: AgentIdentity,
}

/// Input type for a turn shard. Calls flow in hashed by session_id;
/// heartbeats are broadcast to every shard so each can advance its own
/// clock and sweep idle turns without waiting for a new call.
#[derive(Debug, Clone)]
pub enum TurnShardInput {
    Call(AgentCall),
    Heartbeat { ts: i64, stream_id: String },
}

/// Event emitted by the LLM processor for downstream consumption.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    /// A new LLM API request has been detected (for concurrency tracking).
    Start(LlmCallStart),
    /// An LLM API call has been fully completed (request + response paired).
    /// `agent` is `Some` iff an `AgentProfile` matched and extracted session info.
    Complete {
        call: Arc<LlmCall>,
        agent: Option<AgentIdentity>,
    },
    /// Time-advancing heartbeat. Carries `wall_ts_us` (Unix-epoch µs).
    /// Broadcast to all metrics shards so each can close stale windows even
    /// during traffic idle. (Turn shards receive their heartbeats through a
    /// separate `TurnShardInput` so the channel type can stay untyped-call
    /// flavored.)
    Heartbeat { ts: i64, stream_id: String },
}

/// Emitted when an LLM API request is first detected (headers parsed).
/// Used by MetricsAggregator to track concurrency (+1 on start, -1 on complete).
#[derive(Debug, Clone)]
pub struct LlmCallStart {
    pub stream_id: String,
    /// Stable wire-API identifier (see `LlmCall::wire_api`).
    pub wire_api: &'static str,
    pub model: String,
    pub is_stream: bool,
    pub server_ip: IpAddr,
    pub timestamp_us: i64,
}

impl fmt::Display for LlmCallStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[CallStart] {} | {} | stream={} | server={}",
            self.wire_api, self.model, self.is_stream, self.server_ip,
        )
    }
}

/// Information extracted from a wire-API-specific request body.
#[derive(Debug, Clone)]
pub struct RequestInfo {
    pub model: String,
    pub is_stream: bool,
    pub tenant_id: Option<String>,
}

/// Information extracted from a wire-API-specific response (body or SSE).
#[derive(Debug, Clone)]
pub struct ResponseInfo {
    pub model: Option<String>,
    pub finish_reason: Option<FinishReason>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub response_body: Option<String>,
    /// Provider's response/message ID (e.g., "chatcmpl-xxx", "msg_xxx").
    pub response_id: Option<String>,
}

/// Verdict returned by `WireApi::classify_route` — a three-valued outcome
/// so route information can express both "this is me" and "this is not me"
/// without having to re-ask every wire API.
///
/// The registry uses these to short-circuit on `Accept`, skip `Reject`
/// candidates entirely in the shape pass, and defer `Unknown` candidates
/// for structural matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteVerdict {
    /// Method + URI + headers are enough to identify this wire API.
    Accept,
    /// Method + URI + headers are enough to rule this wire API out.
    Reject,
    /// Route alone is ambiguous — defer to `matches_shape`.
    Unknown,
}

/// Trait for wire-API-specific detection + field extraction.
///
/// Each LLM HTTP wire API (OpenAI Chat Completions, OpenAI Responses,
/// Anthropic Messages, etc.) implements this trait to handle its on-wire
/// format differences while producing unified output. Wire APIs are
/// registered in a `WireApiRegistry`, which runs a two-pass detection:
/// `classify_route` first (cheap, inspects method/URI/headers), then
/// `matches_shape` on Unknown candidates (reads the parsed JSON body).
/// Once a wire API wins, the registry uses its extraction methods for the
/// entire request/response lifecycle.
pub trait WireApi: Send + Sync {
    /// Stable identifier (e.g. "anthropic"). Persisted to storage as
    /// `LlmCall.wire_api`; changing this value is a data migration.
    fn name(&self) -> &'static str;

    /// Pass 1 of detection: inspect method + URI + headers only.
    /// No body parsing — this runs on every HTTP request so it must be cheap.
    fn classify_route(&self, req: &ts_protocol::model::HttpRequestData) -> RouteVerdict;

    /// Pass 2 of detection: inspect the parsed JSON body. Called by the
    /// registry only when `classify_route` returned `Unknown` for every
    /// wire API, and the body parses as JSON. `body` is shared across all
    /// candidates (parsed once per request).
    fn matches_shape(
        &self,
        req: &ts_protocol::model::HttpRequestData,
        body: &serde_json::Value,
    ) -> bool;

    /// Extract model, stream flag, tenant from the request.
    fn extract_request(&self, req: &ts_protocol::model::HttpRequestData) -> RequestInfo;

    /// Extract fields from a non-streaming HTTP response body.
    fn extract_response(&self, resp: &ts_protocol::model::HttpResponseData) -> ResponseInfo;

    /// Extract fields from accumulated SSE events (streaming response).
    fn extract_sse(&self, events: &[ts_protocol::model::SseEventData]) -> ResponseInfo;

    /// Structured view of the output: reasoning / message / tool_calls.
    /// Default returns an empty `ParsedOutput`; concrete wire APIs should override.
    fn parse_output(&self, _body: &serde_json::Value) -> ParsedOutput {
        ParsedOutput::default()
    }

    /// Structured view of the input: most-recent user message + tool_results.
    /// Default returns an empty `ParsedInput`; concrete wire APIs should override.
    fn parse_input(&self, _body: &serde_json::Value) -> ParsedInput {
        ParsedInput::default()
    }
}

/// Truncate a string to max_len characters, appending "..." if truncated.
pub fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

/// Structured view of an LLM output extracted from a response body.
/// Per-wire-api implementations of `WireApi::parse_output` produce this.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedOutput {
    pub reasoning: Option<String>,
    pub message: Option<String>,
    pub tool_calls: Vec<ParsedToolCall>,
}

/// Structured view of an LLM input extracted from a request body.
/// Per-wire-api implementations of `WireApi::parse_input` produce this.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedInput {
    /// Most-recent user message in the input, if any.
    pub user_message: Option<String>,
    /// Tool results keyed by the `id` / `call_id` they belong to.
    pub tool_results: Vec<ParsedToolResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

#[cfg(test)]
mod extension_tests {
    use super::*;
    use crate::wire_apis as wa;
    use std::net::IpAddr;
    use std::sync::Arc;

    #[test]
    fn agent_identity_round_trips() {
        let id = AgentIdentity {
            agent_kind: "claude-cli",
            session_id: "sess-1".to_string(),
            turn_id_hint: None,
        };
        assert_eq!(id.agent_kind, "claude-cli");
        assert_eq!(id.session_id, "sess-1");
        assert!(id.turn_id_hint.is_none());
    }

    #[test]
    fn agent_call_carries_arc_and_identity() {
        let call = LlmCall {
            stream_id: String::new(),
            id: "c".into(),
            wire_api: wa::ANTHROPIC,
            model: "claude".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/".into(),
            is_stream: false,
            request_body: None,
            status_code: None,
            finish_reason: None,
            response_body: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        };
        let arc = Arc::new(call);
        let id = AgentIdentity {
            agent_kind: "x",
            session_id: "s".into(),
            turn_id_hint: None,
        };
        let ic = AgentCall {
            call: Arc::clone(&arc),
            agent: id,
        };
        assert_eq!(ic.call.id, "c");
        assert_eq!(ic.agent.session_id, "s");
        assert_eq!(Arc::strong_count(&arc), 2);
    }

    #[test]
    fn llm_call_fields_are_present() {
        let call = LlmCall {
            stream_id: String::new(),
            id: "c1".into(),
            wire_api: wa::ANTHROPIC,
            model: "claude-sonnet".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/messages".into(),
            is_stream: true,
            request_body: None,
            status_code: None,
            finish_reason: None,
            response_body: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        };
        assert!(call.tenant_id.is_none());
        assert!(call.finish_reason.is_none());
    }
}

impl fmt::Display for LlmCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "[LlmCall] {}:{} -> {}:{} | {} | {} | {}",
            self.client_ip,
            self.client_port,
            self.server_ip,
            self.server_port,
            self.wire_api,
            self.model,
            self.request_path,
        )?;
        write!(
            f,
            "  stream={} status={} finish={} response_id={}",
            self.is_stream,
            self.status_code
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".into()),
            self.finish_reason
                .map(|r| r.to_string())
                .unwrap_or_else(|| "-".into()),
            self.response_id.as_deref().unwrap_or("-"),
        )?;
        if self.input_tokens.is_some() || self.output_tokens.is_some() {
            write!(
                f,
                " | tokens: in={} out={} total={}",
                self.input_tokens
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into()),
                self.output_tokens
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into()),
                self.total_tokens
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into()),
            )?;
            if self.cache_read_input_tokens.is_some() || self.cache_creation_input_tokens.is_some()
            {
                write!(
                    f,
                    " cache_read={} cache_create={}",
                    self.cache_read_input_tokens
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-".into()),
                    self.cache_creation_input_tokens
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-".into()),
                )?;
            }
        }
        if self.ttfb_ms.is_some() || self.e2e_latency_ms.is_some() {
            write!(
                f,
                " | ttfb={:.1}ms e2e={:.1}ms",
                self.ttfb_ms.unwrap_or(0.0),
                self.e2e_latency_ms.unwrap_or(0.0),
            )?;
        }
        if let Some(ref body) = self.request_body {
            write!(f, "\n  req: {}", truncate_str(body, 200))?;
        }
        if let Some(ref body) = self.response_body {
            write!(f, "\n  resp: {}", truncate_str(body, 200))?;
        }
        Ok(())
    }
}
