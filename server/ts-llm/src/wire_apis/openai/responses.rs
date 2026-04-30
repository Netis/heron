//! OpenAI Responses wire API (`POST /v1/responses`).
//!
//! Strict to the Responses shape — top-level `status`, `usage.input_tokens`,
//! `usage.output_tokens`, `usage.input_tokens_details.cached_tokens`. No
//! `.or_else()` fallback to the Chat shape: the registry has already
//! selected us, so ambiguity would be a bug.
//!
//! SSE handling keeps the codex-compat grafting: when `response.completed`
//! arrives with an empty `output` array but prior `response.output_item.done`
//! events supplied items, we splice them into the body so downstream
//! consumers always see the full output.

use serde_json::Value;

use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{RequestInfo, ResponseInfo, RouteVerdict, WireApi};

use super::shared::{has_bearer_auth, is_anthropic_request};

pub struct OpenAiResponsesWireApi;

impl WireApi for OpenAiResponsesWireApi {
    fn name(&self) -> &'static str {
        crate::wire_apis::OPENAI_RESPONSES
    }

    fn classify_route(&self, req: &HttpRequestData) -> RouteVerdict {
        if req.method != "POST" {
            return RouteVerdict::Reject;
        }
        if is_anthropic_request(req) {
            return RouteVerdict::Reject;
        }
        let path = req.uri.split('?').next().unwrap_or(&req.uri);
        if path.ends_with("/v1/responses") && has_bearer_auth(req) {
            return RouteVerdict::Accept;
        }
        RouteVerdict::Unknown
    }

    fn matches_shape(&self, _req: &HttpRequestData, body: &Value) -> bool {
        // Responses discriminator: `model` + `input` present, `messages` absent.
        body.get("model").and_then(|v| v.as_str()).is_some()
            && body.get("input").is_some()
            && body.get("messages").is_none()
    }

    fn extract_request(&self, req: &HttpRequestData, body: &Value) -> RequestInfo {
        extract_request(req, body)
    }
    fn extract_response(&self, resp: &HttpResponseData) -> ResponseInfo {
        extract_response(resp)
    }
    fn extract_sse(&self, events: &[SseEventData]) -> ResponseInfo {
        extract_sse(events)
    }

    fn is_terminal(&self, finish_reason: &str) -> bool {
        matches!(
            finish_reason,
            "completed" | "incomplete" | "failed" | "cancelled"
        )
    }

    fn is_tool_use(&self, _finish_reason: &str) -> bool {
        // Responses API surfaces tool use via output items, not finish_reason.
        // Keep predicate false; tracker should rely on output-item inspection.
        false
    }
}

fn extract_request(_req: &HttpRequestData, body: &Value) -> RequestInfo {
    RequestInfo {
        model: body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        is_stream: body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn extract_response(resp: &HttpResponseData) -> ResponseInfo {
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let body_str = std::str::from_utf8(&resp.body).ok().map(|s| s.to_string());

    let response_id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let finish_reason = body
        .get("status")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let usage = body.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let total_tokens = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let cache_read_input_tokens = usage
        .and_then(|u| u.get("input_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens: None,
        response_body: body_str,
        response_id,
    }
}

fn extract_sse(events: &[SseEventData]) -> ResponseInfo {
    let mut model: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut total_tokens: Option<u32> = None;
    let mut cache_read_input_tokens: Option<u32> = None;
    let mut response_body: Option<String> = None;
    let mut response_id: Option<String> = None;
    // Codex streams output items via response.output_item.done; the terminal
    // response.completed often carries an empty `output` array. Accumulate
    // here and graft into the final body below.
    let mut output_items: Vec<Value> = Vec::new();

    for event in events {
        match event.event_type.as_str() {
            "response.created" => {
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(response) = data.get("response") {
                    if response_id.is_none() {
                        if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                            response_id = Some(id.to_string());
                        }
                    }
                    if model.is_none() {
                        if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                            model = Some(m.to_string());
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(item) = data.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(response) = data.get("response") {
                    if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                        response_id = Some(id.to_string());
                    }
                    if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                    if let Some(status) = response.get("status").and_then(|v| v.as_str()) {
                        finish_reason = Some(status.to_string());
                    }
                    if let Some(usage) = response.get("usage") {
                        if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                            input_tokens = Some(it as u32);
                        }
                        if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                            output_tokens = Some(ot as u32);
                        }
                        if let Some(tt) = usage.get("total_tokens").and_then(|v| v.as_u64()) {
                            total_tokens = Some(tt as u32);
                        }
                        if let Some(cr) = usage
                            .get("input_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_input_tokens = Some(cr as u32);
                        }
                    }

                    // Graft accumulated items into the response object when
                    // the wire copy is missing or empty (codex behavior);
                    // otherwise preserve the wire copy verbatim.
                    let mut response_obj = response.clone();
                    let needs_graft = response_obj
                        .get("output")
                        .and_then(|o| o.as_array())
                        .map(|a| a.is_empty())
                        .unwrap_or(true);
                    if needs_graft && !output_items.is_empty() {
                        if let Some(map) = response_obj.as_object_mut() {
                            map.insert("output".to_string(), Value::Array(output_items.clone()));
                        }
                    }
                    response_body = Some(response_obj.to_string());
                }
            }
            // Anything else (including untyped Chat-style chunks) is not ours.
            _ => {}
        }
    }

    ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens: None,
        response_body,
        response_id,
    }
}

/// Decide whether an OpenAI Responses body represents a terminal turn
/// (agent done, no more tool roundtrips pending).
///
/// Logic: scan `response.output[]`. Any item whose `type` ends with `_call`
/// (e.g., `function_call`, `custom_tool_call`, `local_shell_call`, MCP
/// variants) means the agent will execute a tool and re-POST → not terminal.
/// `message` items count as the final answer; `reasoning` is ignored.
/// Return true iff at least one `message` is present and no `*_call` is.
///
/// Used by both `CodexCliProfile` and `GenericProfile` (openai-responses
/// branch) — the OpenAI Responses protocol always sets `status: "completed"`
/// on successful API calls regardless of whether the agent continues, so the
/// wire-api `finish_reason` is unreliable for turn-boundary purposes. This
/// helper is the authoritative override.
///
/// Re-exported via `crate::wire_apis::openai::body_has_terminal_message_only`
/// (the `responses` submodule itself is private). Profiles should use the
/// re-exported path.
pub fn body_has_terminal_message_only(response_body: Option<&str>) -> bool {
    let Some(body) = response_body else {
        return false;
    };
    let Ok(resp) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    body_has_terminal_message_only_value(&resp)
}

/// `Value`-input sibling of `body_has_terminal_message_only`. Used by
/// `AgentProfile::is_turn_terminal` overrides on the parse-once hot path.
pub fn body_has_terminal_message_only_value(resp: &Value) -> bool {
    let Some(output) = resp.get("output").and_then(|o| o.as_array()) else {
        return false;
    };
    let mut has_message = false;
    for item in output {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("message") => has_message = true,
            Some("reasoning") => {}
            Some(t) if t.ends_with("_call") => return false,
            _ => {}
        }
    }
    has_message
}

// ────────────────────────── Body shape parsers ─────────────────────────────
//
// Helpers that walk OpenAI Responses request `input[]` / response
// `output[]` arrays for the pieces agent profiles need. The Responses
// shape is item-oriented (each item has its own `type`), so most helpers
// take `&[Value]` rather than the wrapper `&Value` — agent profiles
// extract the array once via `v.get("input")` / `v.get("output")` and
// pass the slice in.

use crate::wire_apis::AssistantSig;

/// Extract `tools[].name`. Responses tools are FLAT — each entry is
/// `{type:"function", name, parameters, ...}` (no inner `function`
/// wrapper like OpenAI Chat uses). Same `Some(empty) | None` semantics as
/// the Anthropic / OpenAI-Chat counterparts: absent → empty list,
/// wrong shape → `None`.
pub fn tool_names(req: &Value) -> Option<Vec<String>> {
    match req.get("tools") {
        None => Some(Vec::new()),
        Some(Value::Array(arr)) => Some(
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect(),
        ),
        Some(_) => None,
    }
}

/// First system-prompt text in a Responses request. Two placements are
/// possible and must both be checked:
///   1. Top-level `instructions` field (codex-style, mostly a Codex path).
///   2. A `{type:"message", role:"system"|"developer"}` item in `input[]`
///      (the standard non-codex Responses path used by SDKs that fold the
///      system prompt into the conversation).
/// `instructions` wins if present (it's the explicit channel); otherwise
/// fall back to the `input[]` walk.
pub fn first_system_text(req: &Value) -> Option<String> {
    if let Some(s) = req.get("instructions").and_then(|x| x.as_str()) {
        if !s.trim().is_empty() {
            return Some(s.to_string());
        }
    }
    let items = req.get("input")?.as_array()?;
    for it in items {
        if it.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let role = it.get("role").and_then(|v| v.as_str());
        if role != Some("system") && role != Some("developer") {
            continue;
        }
        if let Some(t) = message_text(it) {
            return Some(t);
        }
    }
    None
}

/// Concatenate visible text from a single Responses `message` item's
/// content. Accepts the legacy string shorthand and the canonical
/// `[{"type":"output_text","text":...}]` (or `"input_text"`/`"text"`)
/// array form.
pub fn message_text(item: &Value) -> Option<String> {
    let c = item.get("content")?;
    match c {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    let t = b.get("type").and_then(|v| v.as_str())?;
                    if t == "text" || t == "input_text" || t == "output_text" {
                        b.get("text").and_then(|v| v.as_str()).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

pub fn first_user_text(items: &[Value]) -> Option<String> {
    for it in items {
        if it.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        if it.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        if let Some(t) = message_text(it) {
            return Some(t);
        }
    }
    None
}

/// First-pass scan of `input[]`: prefer the earliest `function_call.call_id`
/// (tool dispatched on a prior turn); fall back to the first assistant
/// `message`'s text. The `function_call` precedence matches Anthropic's
/// `tool_use` precedence in `wire_apis::anthropic::first_assistant_sig_*`.
pub fn first_assistant_sig_from_input(items: &[Value]) -> Option<AssistantSig> {
    let mut tool_id: Option<String> = None;
    let mut text: Option<String> = None;
    for it in items {
        let t = it.get("type").and_then(|v| v.as_str());
        if tool_id.is_none() && t == Some("function_call") {
            if let Some(id) = it.get("call_id").and_then(|v| v.as_str()) {
                tool_id = Some(id.to_string());
            }
        }
        if text.is_none()
            && t == Some("message")
            && it.get("role").and_then(|v| v.as_str()) == Some("assistant")
        {
            if let Some(t) = message_text(it) {
                text = Some(t);
            }
        }
        if tool_id.is_some() && text.is_some() {
            break;
        }
    }
    if let Some(id) = tool_id {
        Some(AssistantSig::ToolId(id))
    } else {
        text.map(AssistantSig::Text)
    }
}

pub fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let resp: Value = serde_json::from_str(body).ok()?;
    first_assistant_sig_from_response_value(&resp)
}

/// `Value`-input sibling of `first_assistant_sig_from_response`. Used by
/// agent-profile session-id extractors on the parse-once hot path.
pub fn first_assistant_sig_from_response_value(resp: &Value) -> Option<AssistantSig> {
    let output = resp.get("output")?.as_array()?;
    first_assistant_sig_from_input(output)
}

/// Extract the latest user-message text in `input[]`. Walks in reverse so
/// multi-turn inputs return the most recent prompt. `input` may be a
/// string (simplified single-prompt mode) or an array (canonical mode);
/// both are handled here so callers don't have to peek at the shape.
pub fn extract_user_input(req: &Value) -> Option<String> {
    match req.get("input")? {
        Value::Array(items) => {
            for it in items.iter().rev() {
                if it.get("type").and_then(|v| v.as_str()) != Some("message") {
                    continue;
                }
                if it.get("role").and_then(|v| v.as_str()) != Some("user") {
                    continue;
                }
                if let Some(t) = message_text(it) {
                    return Some(t);
                }
            }
            None
        }
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// True iff the last item in `input[]` is a non-empty `role=user` message
/// (i.e. user turn, not a tool roundtrip continuation).
/// `function_call_output` items at the tail break user-turn-start.
pub fn is_user_turn_start(req: &Value) -> Option<bool> {
    let items = match req.get("input")? {
        Value::Array(items) => items,
        Value::String(s) => return Some(!s.trim().is_empty()),
        _ => return None,
    };
    let last = items.last()?;
    let t = last.get("type").and_then(|v| v.as_str());
    if t != Some("message") || last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return Some(false);
    }
    Some(message_text(last).is_some())
}

/// First `message` item's text from `output[]`. `None` if the response
/// only contains tool/function calls or `reasoning` items.
pub fn extract_assistant_text(body: &str) -> Option<String> {
    let resp: Value = serde_json::from_str(body).ok()?;
    extract_assistant_text_value(&resp)
}

/// `Value`-input sibling of `extract_assistant_text`. Used by agent-profile
/// extractors on the parse-once hot path.
pub fn extract_assistant_text_value(resp: &Value) -> Option<String> {
    let output = resp.get("output")?.as_array()?;
    for it in output {
        if it.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        if let Some(t) = message_text(it) {
            return Some(t);
        }
    }
    None
}

#[cfg(test)]
mod terminal_helper_tests {
    use super::*;

    #[test]
    fn terminal_when_output_only_has_message() {
        let body = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}]}"#;
        assert!(body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_function_call_present() {
        let body = r#"{"output":[{"type":"message"},{"type":"function_call","call_id":"fc_1"}]}"#;
        assert!(!body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_no_message() {
        let body = r#"{"output":[{"type":"reasoning"}]}"#;
        assert!(!body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_no_body() {
        assert!(!body_has_terminal_message_only(None));
    }

    #[test]
    fn not_terminal_when_malformed_json() {
        assert!(!body_has_terminal_message_only(Some("garbage")));
    }

    #[test]
    fn not_terminal_when_output_array_is_empty() {
        assert!(!body_has_terminal_message_only(Some(r#"{"output":[]}"#)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use ts_protocol::net::FlowKey;

    fn make_sse(event_type: &str, data: &str) -> SseEventData {
        let ip: IpAddr = IpAddr::from([127, 0, 0, 1]);
        SseEventData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            event_type: event_type.to_string(),
            data: data.to_string(),
            timestamp_us: 0,
        }
    }

    #[test]
    fn test_grafts_output_items_into_empty_response_output() {
        // Real-codex behavior: response.completed.response.output is empty;
        // items arrive via response.output_item.done. Parser must accumulate
        // them and graft into the response_body so downstream consumers
        // (turn-terminal predicate, assistant-text extraction) can inspect
        // them.
        let events = vec![
            make_sse(
                "response.created",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"in_progress"}}"#,
            ),
            make_sse(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","summary":[]}}"#,
            ),
            make_sse(
                "response.output_item.done",
                r#"{"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final."}]}}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
            ),
        ];
        let info = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let out = v.get("output").and_then(|o| o.as_array()).unwrap();
        let types: Vec<&str> = out
            .iter()
            .map(|i| i.get("type").and_then(|t| t.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(types, vec!["reasoning", "message"]);
    }

    #[test]
    fn test_preserves_nonempty_response_output() {
        // If response.completed already carries output (older/alternative
        // codex behavior), we must NOT overwrite it with accumulated items.
        let events = vec![
            make_sse(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","summary":[]}}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x"}]}]}}"#,
            ),
        ];
        let info = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let out = v.get("output").and_then(|o| o.as_array()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("type").and_then(|t| t.as_str()), Some("message"));
    }

    #[test]
    fn test_extract_response_id_streaming() {
        let events = vec![
            make_sse(
                "response.created",
                r#"{"response":{"id":"resp_xyz","model":"gpt-4","status":"in_progress"}}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_xyz","model":"gpt-4","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
            ),
        ];
        let info = extract_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("resp_xyz"));
    }

    #[test]
    fn test_extract_sse_cache_read_tokens() {
        // Responses API carries cached_tokens under usage.input_tokens_details.
        let events = vec![make_sse(
            "response.completed",
            r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed","usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110,"input_tokens_details":{"cached_tokens":42}}}}"#,
        )];
        let info = extract_sse(&events);
        assert_eq!(info.cache_read_input_tokens, Some(42));
    }

    #[test]
    fn test_extract_response_non_streaming() {
        // Non-streaming Responses body: same shape as the inner `response`
        // object inside a response.completed SSE event.
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
                "input_tokens_details": {"cached_tokens": 3}
            }
        });
        let resp = HttpResponseData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![],
            body: bytes::Bytes::from(body.to_string()),
            first_byte_timestamp_us: 0,
            complete_timestamp_us: 0,
        };
        let info = extract_response(&resp);
        assert_eq!(info.model.as_deref(), Some("gpt-5"));
        assert_eq!(info.response_id.as_deref(), Some("resp_1"));
        assert_eq!(info.finish_reason.as_deref(), Some("completed"));
        assert_eq!(info.input_tokens, Some(10));
        assert_eq!(info.output_tokens, Some(5));
        assert_eq!(info.total_tokens, Some(15));
        assert_eq!(info.cache_read_input_tokens, Some(3));
    }

    #[test]
    fn predicates_openai_responses() {
        let w = OpenAiResponsesWireApi;
        assert!(w.is_terminal("completed"));
        assert!(w.is_terminal("incomplete"));
        assert!(w.is_terminal("failed"));
        assert!(w.is_terminal("cancelled"));
        assert!(!w.is_terminal("in_progress"));
        assert!(!w.is_terminal("unknown_future_value"));
        // Responses surfaces tool use via output items, not finish_reason —
        // predicate is false even for known terminals.
        assert!(!w.is_tool_use("completed"));
        assert!(!w.is_tool_use("failed"));
        assert!(!w.is_tool_use("tool_calls"));
    }

    #[test]
    fn test_extract_sse_ignores_chat_chunks() {
        // Defensive: if a Chat-style untyped chunk slipped in, we skip it.
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed"}}"#,
            ),
        ];
        let info = extract_sse(&events);
        assert_eq!(info.model.as_deref(), Some("gpt-5"));
        assert_eq!(info.response_id.as_deref(), Some("resp_1"));
    }
}
