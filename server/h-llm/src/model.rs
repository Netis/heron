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

/// A fully extracted LLM API call record.
#[derive(Debug, Clone)]
pub struct LlmCall {
    pub source_id: String,
    pub id: String,
    /// Stable wire-API identifier (e.g. "anthropic", "openai-chat",
    /// "openai-responses"). Sourced from `WireApi::name()`; persisted verbatim
    /// to storage. This is the HTTP API shape, not the vendor.
    pub wire_api: &'static str,
    pub model: String,
    pub api_type: ApiType,
    pub request_time: i64,
    pub response_time: Option<i64>,
    pub complete_time: Option<i64>,
    pub request_path: String,
    pub is_stream: bool,
    pub request_body: Option<String>,
    pub status_code: Option<u16>,
    /// Raw provider finish_reason (`stop_reason` for Anthropic, `finish_reason`
    /// for OpenAI Chat, etc.). Verbatim string from the wire — no normalization.
    /// Use the owning `wire_api` to interpret.
    pub finish_reason: Option<String>,
    pub response_body: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub ttft_ms: Option<f64>,
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
    /// True when this call is part of an agent loop. Copied from
    /// `AgentCallInfo.is_agent_request` in the LLM stage for storage.
    pub is_agent_request: bool,
    /// Tool-invocation surface for this call. Copied from `AgentCallInfo`.
    pub tool_surface: Option<h_common::agent::ToolSurface>,
    /// Agent topology role for this call. Copied from `AgentCallInfo`.
    pub agent_topology: Option<h_common::agent::AgentTopology>,
    /// Count of tool calls in this call. Copied from `AgentCallInfo`.
    pub tool_call_count: u32,
    /// Distinct tool names referenced in this call. Copied from `AgentCallInfo`.
    pub tool_names: Vec<String>,
    /// Bytes elided from the stored `request_body` + `response_body` by the
    /// body-cap policy (`BodyCapConfig`). `0` when nothing was dropped (body
    /// fit the budget or the cap was disabled). Non-zero exactly when the
    /// stored bodies were sampled (head + tail retained, middle elided).
    /// Extraction (usage/model) always runs on the full body upstream, so a
    /// non-zero value never implies lost metrics — only a truncated stored body.
    pub body_bytes_dropped: u64,
    /// Owning process (pid / comm / exe), when the capture source attributes it
    /// (eBPF). `None` for passive taps (pcap / cloud-probe). Copied from the
    /// request's process attribution (falling back to the response's) in the
    /// LLM stage, persisted to storage, and surfaced in the console so a call
    /// can be traced back to the agent process that made it.
    pub process: Option<h_common::process::ProcessInfo>,
}

/// Per-call agent-side info produced once an `AgentProfile` has matched —
/// i.e. the call is attributed to a known agent client (claude-cli, codex-cli,
/// …). Non-agent traffic never produces an `AgentCallInfo`.
///
/// Carries the full per-call classification computed once at the h-llm
/// boundary (identity + verdicts + extracted text). Downstream stages (turn
/// assembly) read fields and never re-invoke profile predicates — keeps
/// cross-cutting filters (sub-agent vs main) at one site instead of re-applied
/// at every consumer.
#[derive(Debug, Clone)]
pub struct AgentCallInfo {
    /// Short stable agent name (e.g. `"claude-cli"`). Doubles as the profile
    /// selector (look up via `AgentProfileRegistry::find_by_name`) and the
    /// persisted `AgentTurn.agent_kind` storage value.
    pub agent_kind: &'static str,
    pub session_id: String,
    /// `Some(name)` if this call belongs to a sub-agent (e.g. `"task"` for
    /// Claude Task tool, the explicit header value for Codex). `None` ⇒
    /// main-agent. Derived from `AgentProfile::subagent`.
    pub subagent_name: Option<String>,
    /// Raw structural verdict from `AgentProfile::is_user_turn_start`. Sub-agent
    /// filtering is NOT applied here — consumers must also check
    /// `subagent_name.is_none()` if they want main-agent user starts only.
    pub is_user_turn_start: Option<bool>,
    /// True iff this call's protocol semantics close the agent's current
    /// turn. Profile-and-wire-api dispatch only — the profile decides, with
    /// the trait's default impl handling the implicit wire-api path. **Sub-
    /// agent layering is NOT applied here.** Consumers that want "this call
    /// closes the *main* agent's turn" must combine
    /// `subagent_name.is_none() && is_turn_terminal`, the same way they
    /// combine `subagent_name.is_none() && is_user_turn_start == Some(true)`
    /// for main-agent user starts.
    pub is_turn_terminal: bool,
    /// True iff this call is an auxiliary one-shot (e.g. claude-cli session
    /// title generation) that should bypass turn assembly entirely.
    pub is_auxiliary: bool,
    /// Full user prompt text extracted from the request body, with profile-
    /// specific scaffolding stripped. Eagerly computed (matches the prior
    /// per-call extraction pattern in tracker). `None` when body is absent
    /// or yields no user text.
    pub user_input: Option<String>,
    /// Full assistant text extracted from the response body. `None` when
    /// body is absent or yields no assistant text.
    pub assistant_text: Option<String>,
    /// True when this call shows signs of being part of an agent loop
    /// (tool calls present, sub-agent marker, or agent-shaped system prompt).
    /// Set by `agent_classifier::classify` from extractor-emitted primitives.
    pub is_agent_request: bool,
    /// Tool-invocation surface for this call, or None when no tool calls present.
    pub tool_surface: Option<h_common::agent::ToolSurface>,
    /// This call's role in its turn's agent topology. None when not an agent request.
    pub agent_topology: Option<h_common::agent::AgentTopology>,
    /// Distinct tool names referenced in this call (preserved in order of first
    /// appearance). Empty when no tool calls present.
    pub tool_names: Vec<String>,
    /// Count of tool calls in this call. Mirrors `tool_names.len()` after dedup
    /// when extractors are well-behaved; stored independently because some
    /// profiles can count calls without naming them.
    pub tool_call_count: u32,
    /// Suspicious-skill signals detected for this call.
    pub suspicious_signals: Vec<crate::agent_classifier::SuspiciousSignal>,
}

/// An LlmCall attributed to a specific agent. The `call` is an `Arc` because
/// the same `LlmCall` is fanned out to the storage sink and the turn shard in
/// parallel — all consumers read-only, no mutex needed.
#[derive(Debug, Clone)]
pub struct AgentCall {
    pub call: Arc<LlmCall>,
    pub agent: AgentCallInfo,
}

/// Input type for a turn shard. Calls flow in hashed by session_id;
/// heartbeats are broadcast to every shard so each can advance its own
/// clock and sweep idle turns without waiting for a new call.
#[derive(Debug, Clone)]
pub enum TurnShardInput {
    Call(AgentCall),
    Heartbeat { ts: i64, source_id: String },
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
        agent: Option<AgentCallInfo>,
    },
    /// Time-advancing heartbeat. Carries `wall_ts_us` (Unix-epoch µs).
    /// Broadcast to all metrics shards so each can close stale windows even
    /// during traffic idle. (Turn shards receive their heartbeats through a
    /// separate `TurnShardInput` so the channel type can stay untyped-call
    /// flavored.)
    Heartbeat { ts: i64, source_id: String },
}

/// Emitted when an LLM API request is first detected (headers parsed).
/// Used by MetricsAggregator to track concurrency (+1 on start, -1 on complete).
#[derive(Debug, Clone)]
pub struct LlmCallStart {
    pub source_id: String,
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
}

/// Information extracted from a wire-API-specific response (body or SSE).
#[derive(Debug, Clone)]
pub struct ResponseInfo {
    pub model: Option<String>,
    pub finish_reason: Option<String>,
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
    fn classify_route(&self, req: &h_protocol::model::HttpRequestData) -> RouteVerdict;

    /// Pass 2 of detection: inspect the request body via the shared
    /// parse cache. The first candidate that calls `req_body.get()`
    /// triggers the parse; the rest hit the cached `Value`.
    fn matches_shape(
        &self,
        req: &h_protocol::model::HttpRequestData,
        req_body: &crate::parsed_json::ParsedJson,
    ) -> bool;

    /// Extract model and stream flag from the request.
    ///
    /// `req_body` is the per-event request-body parse cache, bound to
    /// `req.body` at construction by the caller. Implementations that
    /// need a `Value` call `req_body.get()`; within the current event
    /// (a single Start or Complete pass through the processor) the cache
    /// guarantees the body is parsed at most once across detection +
    /// agent classification. The Start and Complete paths use independent
    /// caches — see `parsed_json` module docs. Path-only Accept routes
    /// whose `extract_request` doesn't read the body never trigger a
    /// parse.
    fn extract_request(
        &self,
        req: &h_protocol::model::HttpRequestData,
        req_body: &crate::parsed_json::ParsedJson,
    ) -> RequestInfo;

    /// Extract fields from a non-streaming HTTP response body.
    /// Implementations call `resp_body.get()`; the resulting `Value` is
    /// also available downstream via the same cache without re-parsing.
    fn extract_response(
        &self,
        resp: &h_protocol::model::HttpResponseData,
        resp_body: &crate::parsed_json::ParsedJson,
    ) -> ResponseInfo;

    /// Extract fields from accumulated SSE events (streaming response).
    /// Implementations parse each event once, assemble a synthetic body
    /// `Value`, and return both the `ResponseInfo` and a body-bound
    /// `ParsedJson::from_value(Some(value))` cache so downstream readers
    /// see the same `Value` without a String→Value round-trip. When the
    /// stream produced no body (zero usable events), the cache is
    /// `ParsedJson::from_value(None)`. Total parses for an SSE response:
    /// N events; no extra round-trip.
    fn extract_sse(
        &self,
        events: &[h_protocol::model::SseEventData],
    ) -> (ResponseInfo, crate::parsed_json::ParsedJson);

    /// True iff `finish_reason` is a wire-level terminal — i.e. the model has
    /// finished emitting this message and the agent loop must decide whether to
    /// continue (e.g. tool result) or finalize. Anthropic `pause_turn` is NOT
    /// terminal: the assistant turn continues after the server-tool loop yields.
    fn is_terminal(&self, finish_reason: &str) -> bool;

    /// True iff `finish_reason` indicates the model is requesting tool execution
    /// and expects a tool_result message in the next turn.
    fn is_tool_use(&self, finish_reason: &str) -> bool;
}

/// Truncate a string to max_len characters, appending "..." if truncated.
pub fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

#[cfg(test)]
mod extension_tests {
    use super::*;
    use crate::wire_apis as wa;
    use std::net::IpAddr;
    use std::sync::Arc;

    #[test]
    fn agent_call_info_round_trips() {
        let id = AgentCallInfo {
            agent_kind: "claude-cli",
            session_id: "sess-1".to_string(),
            subagent_name: None,
            is_user_turn_start: None,
            is_turn_terminal: false,
            is_auxiliary: false,
            user_input: None,
            assistant_text: None,
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_names: Vec::new(),
            tool_call_count: 0,
            suspicious_signals: Vec::new(),
        };
        assert_eq!(id.agent_kind, "claude-cli");
        assert_eq!(id.session_id, "sess-1");
        assert!(id.subagent_name.is_none());
    }

    #[test]
    fn agent_call_carries_arc_and_info() {
        let call = LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api: wa::ANTHROPIC,
            model: "claude".into(),
            api_type: ApiType::Chat,
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
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
            body_bytes_dropped: 0,
            process: None,
        };
        let arc = Arc::new(call);
        let id = AgentCallInfo {
            agent_kind: "x",
            session_id: "s".into(),
            subagent_name: None,
            is_user_turn_start: None,
            is_turn_terminal: false,
            is_auxiliary: false,
            user_input: None,
            assistant_text: None,
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_names: Vec::new(),
            tool_call_count: 0,
            suspicious_signals: Vec::new(),
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
            source_id: String::new(),
            id: "c1".into(),
            wire_api: wa::ANTHROPIC,
            model: "claude-sonnet".into(),
            api_type: ApiType::Chat,
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
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
            body_bytes_dropped: 0,
            process: None,
        };
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
            self.finish_reason.as_deref().unwrap_or("-"),
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
        if self.ttft_ms.is_some() || self.e2e_latency_ms.is_some() {
            write!(
                f,
                " | ttft={:.1}ms e2e={:.1}ms",
                self.ttft_ms.unwrap_or(0.0),
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
