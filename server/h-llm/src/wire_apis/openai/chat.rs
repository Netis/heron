//! OpenAI Chat Completions wire API (`POST /v1/chat/completions`).
//!
//! Strict to the Chat shape — `choices[0].finish_reason`,
//! `usage.prompt_tokens`, `usage.completion_tokens`,
//! `usage.prompt_tokens_details.cached_tokens`. No `.or_else()` fallback to
//! the Responses shape: this module only runs once the registry has already
//! selected us, so ambiguity would be a bug, not something to tolerate.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use h_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{RequestInfo, ResponseInfo, RouteVerdict, WireApi};
use crate::parsed_json::ParsedJson;

use super::shared::{has_bearer_auth, is_anthropic_request};

pub struct OpenAiChatWireApi;

impl WireApi for OpenAiChatWireApi {
    fn name(&self) -> &'static str {
        crate::wire_apis::OPENAI_CHAT
    }

    fn classify_route(&self, req: &HttpRequestData) -> RouteVerdict {
        if req.method != "POST" {
            return RouteVerdict::Reject;
        }
        if is_anthropic_request(req) {
            return RouteVerdict::Reject;
        }
        let path = req.uri.split('?').next().unwrap_or(&req.uri);
        if path.ends_with("/v1/chat/completions") && has_bearer_auth(req) {
            return RouteVerdict::Accept;
        }
        RouteVerdict::Unknown
    }

    fn matches_shape(&self, _req: &HttpRequestData, req_body: &ParsedJson) -> bool {
        let Some(req_body) = req_body.get() else {
            return false;
        };
        // Chat Completions: `model` + non-empty `messages[]`. Presence of
        // `input` means the Responses API, not us.
        req_body.get("model").and_then(|v| v.as_str()).is_some()
            && req_body.get("input").is_none()
            && req_body
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
    }

    fn extract_request(&self, req: &HttpRequestData, req_body: &ParsedJson) -> RequestInfo {
        extract_request(req, req_body)
    }
    fn extract_response(&self, resp: &HttpResponseData, resp_body: &ParsedJson) -> ResponseInfo {
        extract_response(resp, resp_body)
    }
    fn extract_sse(&self, events: &[SseEventData]) -> (ResponseInfo, ParsedJson) {
        extract_sse(events)
    }

    fn is_terminal(&self, finish_reason: &str) -> bool {
        matches!(
            finish_reason,
            "stop" | "length" | "tool_calls" | "function_call" | "content_filter"
        )
    }

    fn is_tool_use(&self, finish_reason: &str) -> bool {
        matches!(finish_reason, "tool_calls" | "function_call")
    }
}

fn extract_request(_req: &HttpRequestData, req_body: &ParsedJson) -> RequestInfo {
    let body = req_body.get();
    RequestInfo {
        model: body
            .and_then(|b| b.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        // Chat Completions defaults to non-streaming; explicit opt-in via "stream": true.
        is_stream: body
            .and_then(|b| b.get("stream"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn extract_response(resp: &HttpResponseData, resp_body: &ParsedJson) -> ResponseInfo {
    let parsed = resp_body.get();
    let body_str = std::str::from_utf8(&resp.body).ok().map(|s| s.to_string());

    let response_id = parsed
        .and_then(|b| b.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model = parsed
        .and_then(|b| b.get("model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let finish_reason = parsed
        .and_then(|b| b.get("choices"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let usage = parsed.and_then(|b| b.get("usage"));
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let total_tokens = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let cache_read_input_tokens = usage
        .and_then(|u| u.get("prompt_tokens_details"))
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

/// Walk Chat Completions SSE chunks in a single pass, accumulating both the
/// numerical fields and the deltas needed to reconstruct a non-streaming
/// `choices[0].message` response. Events with a non-empty `event_type` are
/// Responses events and are silently skipped.
///
/// Returns the `ResponseInfo` plus a body-bound `ParsedJson` cache built
/// from the same synthetic `Value`. The string form is serialized once
/// for storage; downstream readers (the agent boundary) read the cache
/// directly — no String→Value round-trip ever happens.
fn extract_sse(events: &[SseEventData]) -> (ResponseInfo, ParsedJson) {
    let mut response_id: Option<String> = None;
    let mut model: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut content = String::new();
    // Reasoning trace channels. Most reasoning-capable backends emit one of:
    //   * `delta.reasoning_content` — DeepSeek-R1, Qwen3, Qwen3.5/3.6 with
    //     `--reasoning-parser qwen3` on vLLM ≤ 0.16, SGLang.
    //   * `delta.reasoning`         — vLLM ≥ 0.17 (field rename), some GLM
    //     deployments.
    // We accumulate them into separate buffers and emit both into the
    // synthetic final message so `collect_chat_assistant_text` /
    // `estimate_output_tokens` / the console renderer all see what the
    // model actually sent.
    let mut reasoning_content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut total_tokens: Option<u32> = None;
    let mut cache_read_input_tokens: Option<u32> = None;
    let mut saw_chunk = false;

    for event in events {
        if !event.event_type.is_empty() {
            continue; // Responses event, not ours
        }
        let data: Value = match serde_json::from_str::<Value>(&event.data) {
            Ok(v) if v.is_object() => v,
            _ => continue, // [DONE] sentinel or malformed
        };
        saw_chunk = true;

        if response_id.is_none() {
            if let Some(id) = data.get("id").and_then(|v| v.as_str()) {
                response_id = Some(id.to_string());
            }
        }
        if model.is_none() {
            if let Some(m) = data.get("model").and_then(|v| v.as_str()) {
                model = Some(m.to_string());
            }
        }

        if let Some(choice) = data.get("choices").and_then(|c| c.get(0)) {
            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                finish_reason = Some(fr.to_string());
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                    content.push_str(c);
                }
                if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                    reasoning_content.push_str(r);
                }
                if let Some(r) = delta.get("reasoning").and_then(|v| v.as_str()) {
                    reasoning.push_str(r);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                        let entry = tool_calls
                            .entry(idx)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            entry.0 = id.to_string();
                        }
                        if let Some(f) = tc.get("function") {
                            if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                                entry.1 = name.to_string();
                            }
                            if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
            }
        }

        // Final chunk may carry usage (stream_options: {"include_usage": true}).
        if input_tokens.is_none() {
            if let Some(usage) = data.get("usage").filter(|u| u.is_object()) {
                input_tokens = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                output_tokens = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                total_tokens = usage
                    .get("total_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                cache_read_input_tokens = usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
            }
        }
    }

    let synthetic = if saw_chunk {
        Some(build_synthetic_body(
            model.as_deref(),
            &content,
            &reasoning_content,
            &reasoning,
            tool_calls,
            finish_reason.as_deref(),
            input_tokens,
            output_tokens,
            total_tokens,
            cache_read_input_tokens,
        ))
    } else {
        None
    };
    let response_body = synthetic.as_ref().map(Value::to_string);
    let response_cache = ParsedJson::from_value(synthetic);

    let info = ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens: None,
        response_body,
        response_id,
    };
    (info, response_cache)
}

/// Compose a non-streaming-shaped response body from accumulated deltas so
/// downstream consumers don't need a separate streaming reader. Returns a
/// `Value`; callers serialize once to `String` for storage and hand the
/// same `Value` to the per-call `ParsedJson` cache so profile methods read
/// it without re-parsing.
#[allow(clippy::too_many_arguments)]
fn build_synthetic_body(
    model: Option<&str>,
    content: &str,
    reasoning_content: &str,
    reasoning: &str,
    tool_calls: BTreeMap<u64, (String, String, String)>,
    finish_reason: Option<&str>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    total_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
) -> Value {
    let mut message = json!({ "role": "assistant" });
    if !content.is_empty() {
        message["content"] = Value::String(content.to_string());
    }
    // Preserve both reasoning channels separately. Some backends emit one,
    // some the other, a few echo both with identical text; `token_estimator`
    // already dedupes when both are present, and the console renderer keys
    // off `reasoning_content` first.
    if !reasoning_content.is_empty() {
        message["reasoning_content"] = Value::String(reasoning_content.to_string());
    }
    if !reasoning.is_empty() {
        message["reasoning"] = Value::String(reasoning.to_string());
    }
    if !tool_calls.is_empty() {
        let tc_array: Vec<Value> = tool_calls
            .into_iter()
            .map(|(_, (id, name, args))| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(tc_array);
    }

    let mut result = json!({
        "model": model.unwrap_or_default(),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
    });

    if input_tokens.is_some() || output_tokens.is_some() || total_tokens.is_some() {
        let mut usage = serde_json::Map::new();
        if let Some(it) = input_tokens {
            usage.insert("prompt_tokens".to_string(), Value::from(it));
        }
        if let Some(ot) = output_tokens {
            usage.insert("completion_tokens".to_string(), Value::from(ot));
        }
        if let Some(tt) = total_tokens {
            usage.insert("total_tokens".to_string(), Value::from(tt));
        }
        if let Some(cr) = cache_read_input_tokens {
            usage.insert(
                "prompt_tokens_details".to_string(),
                json!({ "cached_tokens": cr }),
            );
        }
        if let Some(obj) = result.as_object_mut() {
            obj.insert("usage".to_string(), Value::Object(usage));
        }
    }

    result
}

// ────────────────────────── Body shape parsers ─────────────────────────────
//
// Helpers that walk OpenAI Chat Completions request / response bodies for
// the pieces agent profiles need (first/last user text, assistant
// signature, tool names, system prompt). Mirror the surface in
// `wire_apis::anthropic` — agent profiles dispatch on `wire_api` and call
// the corresponding helper.

use crate::wire_apis::AssistantSig;

/// OpenAI Chat user content can be a plain string or a list of content
/// blocks (`{"type":"text","text":...}` or the legacy `"input_text"`
/// alias). Returns the concatenated visible text, or `None` if the input
/// is missing / empty / wrong-typed.
pub fn user_content_to_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    let t = b.get("type").and_then(|v| v.as_str())?;
                    if t == "text" || t == "input_text" {
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

/// Extract `tools[].function.name`. Same `Some(empty) | None` semantics as
/// `wire_apis::anthropic::tool_names`: absent → empty list (parseable, no
/// tools), wrong shape → `None` (not parseable).
pub fn tool_names(req: &Value) -> Option<Vec<String>> {
    match req.get("tools") {
        None => Some(Vec::new()),
        Some(Value::Array(arr)) => Some(
            arr.iter()
                .filter_map(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .map(str::to_string)
                })
                .collect(),
        ),
        Some(_) => None,
    }
}

/// OpenAI Chat puts the system prompt as a `role:"system"` message at the
/// head of `messages[]` — there's no top-level `system` field.
pub fn first_system_text(req: &Value) -> Option<String> {
    let msgs = req.get("messages")?.as_array()?;
    for m in msgs {
        if m.get("role").and_then(|r| r.as_str()) != Some("system") {
            continue;
        }
        return user_content_to_text(m.get("content")?);
    }
    None
}

/// Text of the **first** `role:user` message only. If that message has no
/// extractable text, return `None` instead of scanning later user messages
/// — those belong to subsequent turns and aren't a stable session anchor.
pub fn first_user_text(req: &Value) -> Option<String> {
    let msgs = req.get("messages")?.as_array()?;
    let first_user = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    first_user.get("content").and_then(user_content_to_text)
}

/// Sig of the **first** `role:assistant` message — and only that one. If
/// the first assistant has no usable `tool_calls[0].id` AND no non-empty
/// `content` string, return `None` rather than falling through to later
/// assistant messages (which would belong to turn 2+ and are not a stable
/// session anchor). Within that one message, `tool_calls` takes precedence
/// over `content`.
pub fn first_assistant_sig_from_request(req: &Value) -> Option<AssistantSig> {
    let msgs = req.get("messages")?.as_array()?;
    let first_assistant = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))?;
    if let Some(arr) = first_assistant.get("tool_calls").and_then(|v| v.as_array()) {
        if let Some(id) = arr
            .first()
            .and_then(|tc| tc.get("id"))
            .and_then(|v| v.as_str())
        {
            return Some(AssistantSig::ToolId(id.to_string()));
        }
    }
    let c = first_assistant.get("content").and_then(|v| v.as_str())?;
    if c.trim().is_empty() {
        None
    } else {
        Some(AssistantSig::Text(c.to_string()))
    }
}

pub fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let resp: Value = serde_json::from_str(body).ok()?;
    first_assistant_sig_from_response_value(&resp)
}

/// `Value`-input sibling of `first_assistant_sig_from_response`. Used by
/// agent-profile session-id extractors on the parse-once hot path.
pub fn first_assistant_sig_from_response_value(resp: &Value) -> Option<AssistantSig> {
    let msg = resp.get("choices")?.get(0)?.get("message")?;
    if let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        if let Some(id) = arr
            .first()
            .and_then(|tc| tc.get("id"))
            .and_then(|v| v.as_str())
        {
            return Some(AssistantSig::ToolId(id.to_string()));
        }
    }
    if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
        if !c.trim().is_empty() {
            return Some(AssistantSig::Text(c.to_string()));
        }
    }
    None
}

/// Last user message's text. `None` if last is not role=user or content
/// has no visible text. Equivalent to `AgentProfile::extract_user_input`
/// for OpenAI-Chat-shape bodies. Tool results in Chat Completions are
/// `role:"tool"` messages, so the role check naturally excludes them.
pub fn extract_user_input(req: &Value) -> Option<String> {
    let last = req.get("messages")?.as_array()?.last()?;
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return None;
    }
    last.get("content").and_then(user_content_to_text)
}

/// True iff the last message is role=user with non-empty visible text.
/// Equivalent to `AgentProfile::is_user_turn_start` for OpenAI-Chat
/// bodies. (Tool results are role=tool, so the role check alone suffices —
/// no per-block filtering needed.)
pub fn is_user_turn_start(req: &Value) -> Option<bool> {
    let last = req.get("messages")?.as_array()?.last()?;
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return Some(false);
    }
    Some(last.get("content").and_then(user_content_to_text).is_some())
}

pub fn extract_assistant_text(body: &str) -> Option<String> {
    let resp: Value = serde_json::from_str(body).ok()?;
    extract_assistant_text_value(&resp)
}

/// `Value`-input sibling of `extract_assistant_text`. Used by
/// agent-profile extractors on the parse-once hot path.
pub fn extract_assistant_text_value(resp: &Value) -> Option<String> {
    let c = resp
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?;
    if c.trim().is_empty() {
        None
    } else {
        Some(c.to_string())
    }
}

// ─────────────────────────── Token estimation ─────────────────────────────
//
// Fallback estimators for when the response payload omits the `usage` field.
// These walk the same body shapes as the parsers above and feed every
// distinct piece of text through the supplied `TokenEstimator`. They exist
// alongside (not on top of) the wire-usage parsing — the live processor only
// invokes them when the wire `usage` block was absent.

use crate::token_estimator::{collect_chat_assistant_text, TokenEstimator};

/// True iff the response body carried any Chat-shape `usage` field. The API
/// handler uses this against `response_body` to decide `tokens_estimated`.
pub fn usage_present(body: &Value) -> bool {
    let u = match body.get("usage") {
        Some(v) => v,
        None => return false,
    };
    if !u.is_object() {
        return false;
    }
    u.get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .map(|n| n > 0)
        .unwrap_or(false)
        || u.get("completion_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n > 0)
            .unwrap_or(false)
        || u.get("total_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n > 0)
            .unwrap_or(false)
}

/// Walk the assistant message on choices[0].message and produce a single
/// concatenated string with reasoning_content / reasoning / `<think>` /
/// content / tool_calls all deduplicated. Returns empty if the response
/// shape is unexpected.
pub fn collected_response_text(body: &Value) -> String {
    let msg = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"));
    match msg {
        Some(m) => collect_chat_assistant_text(m),
        None => String::new(),
    }
}

/// Estimate input tokens for an OpenAI Chat request body. Walks every
/// message (system / user / assistant / tool roles) plus the `tools[*]`
/// schema. Empty / wrong-shaped bodies return 0.
pub fn estimate_input_tokens(req: &Value, est: &dyn TokenEstimator) -> u32 {
    let mut total: u32 = 0;
    if let Some(msgs) = req.get("messages").and_then(|v| v.as_array()) {
        for m in msgs {
            // role token (small but stable on the wire)
            if let Some(role) = m.get("role").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(role));
            }
            // content: string or array
            if let Some(s) = m.get("content").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(s));
            } else if let Some(arr) = m.get("content").and_then(|v| v.as_array()) {
                for part in arr {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                        total = total.saturating_add(est.count_text(t));
                    }
                }
            }
            // assistant tool_calls (counts as the model output it would have
            // generated had this been part of a fresh response)
            if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    if let Ok(s) = serde_json::to_string(tc) {
                        total = total.saturating_add(est.count_text(&s));
                    }
                }
            }
            // tool result message: tool_call_id is small, content already covered.
        }
    }
    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            if let Ok(s) = serde_json::to_string(t) {
                total = total.saturating_add(est.count_text(&s));
            }
        }
    }
    if let Some(s) = req.get("tool_choice").and_then(|v| v.as_str()) {
        total = total.saturating_add(est.count_text(s));
    }
    total
}

/// Estimate output tokens for an OpenAI Chat response body. Counts all
/// reasoning channels + content + tool_calls, deduplicated.
pub fn estimate_output_tokens(resp: &Value, est: &dyn TokenEstimator) -> u32 {
    let text = collected_response_text(resp);
    est.count_text(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use h_protocol::net::FlowKey;

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
    fn test_synthetic_body_text() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":" world"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
            make_sse("", "[DONE]"),
        ];
        let (info, _body) = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "gpt-4");
        assert_eq!(v["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn synthetic_body_captures_streamed_reasoning_content() {
        // DeepSeek-R1 / Qwen3 native shape: reasoning trace arrives in
        // `delta.reasoning_content`, then the final answer in `delta.content`.
        let events = vec![
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Let me think"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{"reasoning_content":" about this..."}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{"content":"42"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let msg = &v["choices"][0]["message"];
        assert_eq!(msg["content"], "42");
        assert_eq!(msg["reasoning_content"], "Let me think about this...");
        assert!(
            msg.get("reasoning").is_none(),
            "should not invent a reasoning field"
        );
    }

    #[test]
    fn synthetic_body_captures_streamed_reasoning_alias() {
        // vLLM ≥ 0.17 renamed the streaming field to `delta.reasoning`.
        let events = vec![
            make_sse(
                "",
                r#"{"model":"glm-5","choices":[{"index":0,"delta":{"role":"assistant","reasoning":"step one,"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"glm-5","choices":[{"index":0,"delta":{"reasoning":" step two"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"glm-5","choices":[{"index":0,"delta":{"content":"done"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"glm-5","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let msg = &v["choices"][0]["message"];
        assert_eq!(msg["content"], "done");
        assert_eq!(msg["reasoning"], "step one, step two");
        assert!(msg.get("reasoning_content").is_none());
    }

    #[test]
    fn synthetic_body_streamed_reasoning_inflates_output_tokens() {
        // Without server-reported usage, estimate_output_tokens walks the
        // synthetic message — it must see reasoning_content, otherwise the
        // total_output_tokens metric undercounts reasoning models.
        use crate::token_estimator::TokenEstimator;
        struct CharLen;
        impl TokenEstimator for CharLen {
            fn count_text(&self, s: &str) -> u32 {
                s.chars().count() as u32
            }
        }
        let events_with = vec![
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{"reasoning_content":"abcdefghij"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{"content":"42"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let (info_with, _) = extract_sse(&events_with);
        let v_with: Value = serde_json::from_str(&info_with.response_body.unwrap()).unwrap();
        let with_n = estimate_output_tokens(&v_with, &CharLen);

        let events_without = vec![
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{"content":"42"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"qwen3","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let (info_without, _) = extract_sse(&events_without);
        let v_without: Value = serde_json::from_str(&info_without.response_body.unwrap()).unwrap();
        let plain_n = estimate_output_tokens(&v_without, &CharLen);

        assert!(
            with_n > plain_n,
            "streamed reasoning_content must inflate output tokens (with={with_n} plain={plain_n})"
        );
    }

    #[test]
    fn test_synthetic_body_tool_calls() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"SF\"}"}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let tc = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["function"]["name"], "get_weather");
        // arguments must be a string, not a parsed object
        assert!(tc["function"]["arguments"].is_string());
        assert_eq!(tc["function"]["arguments"], r#"{"city":"SF"}"#);
    }

    #[test]
    fn test_synthetic_body_empty() {
        let (info, body) = extract_sse(&[]);
        assert!(info.response_body.is_none());
        // Empty stream → no synthetic Value bound to the cache.
        assert!(body.get().is_none());
    }

    #[test]
    fn test_synthetic_body_includes_usage() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["usage"]["prompt_tokens"], 10);
        assert_eq!(v["usage"]["completion_tokens"], 5);
        assert_eq!(v["usage"]["total_tokens"], 15);
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 3);
    }

    #[test]
    fn test_extract_request_stream_default_false() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let body = json!({"model": "gpt-4"});
        let req = HttpRequestData {
            flow_key: FlowKey::new(String::new(), ip, 1000, ip, 8080),
            client_addr: (ip, 1000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            version: 1,
            headers: vec![("authorization".to_string(), "Bearer sk-test".to_string())],
            body: bytes::Bytes::from(body.to_string()),
            timestamp_us: 0,
        };
        let cache = ParsedJson::from_bytes(req.body.clone());
        let info = extract_request(&req, &cache);
        assert!(
            !info.is_stream,
            "stream should default to false for Chat Completions"
        );
    }

    #[test]
    fn test_extract_sse_finish_reason_stop() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
            make_sse("", "[DONE]"),
        ];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn test_extract_sse_finish_reason_tool_calls() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"f","arguments":""}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn test_extract_sse_usage() {
        // Chat Completions final chunk with usage (stream_options: include_usage).
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.input_tokens, Some(10));
        assert_eq!(info.output_tokens, Some(5));
        assert_eq!(info.total_tokens, Some(15));
    }

    #[test]
    fn test_extract_response_id_non_streaming() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let body = json!({
            "id": "chatcmpl-abc123",
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
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
        let cache = ParsedJson::from_bytes(resp.body.clone());
        let info = extract_response(&resp, &cache);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-abc123"));
    }

    #[test]
    fn test_extract_response_id_streaming() {
        let events = vec![
            make_sse(
                "",
                r#"{"id":"chatcmpl-stream1","model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            ),
            make_sse(
                "",
                r#"{"id":"chatcmpl-stream1","model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"id":"chatcmpl-stream1","model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-stream1"));
    }

    #[test]
    fn test_extract_sse_cache_read_tokens() {
        // Chat Completions final chunk carries cached_tokens under
        // usage.prompt_tokens_details. Must still be read without the
        // Responses-shape fallback.
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":7}}}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.cache_read_input_tokens, Some(7));
    }

    #[test]
    fn predicates_openai_chat() {
        let w = OpenAiChatWireApi;
        assert!(w.is_terminal("stop"));
        assert!(w.is_terminal("length"));
        assert!(w.is_terminal("tool_calls"));
        assert!(w.is_terminal("function_call"));
        assert!(w.is_terminal("content_filter"));
        assert!(!w.is_terminal("unknown_future_value"));
        assert!(w.is_tool_use("tool_calls"));
        assert!(w.is_tool_use("function_call"));
        assert!(!w.is_tool_use("stop"));
    }

    // ─────────── Token estimator walker tests (Step 2) ───────────

    use crate::token_estimator::CL100kEstimator;

    fn cl() -> CL100kEstimator {
        CL100kEstimator::new()
    }

    #[test]
    fn usage_present_true_when_prompt_tokens_set() {
        let body = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5}});
        assert!(usage_present(&body));
    }

    #[test]
    fn usage_present_false_when_missing() {
        let body = json!({"choices":[]});
        assert!(!usage_present(&body));
    }

    #[test]
    fn usage_present_false_when_zero_only() {
        // LiteLLM proxy sometimes emits an empty usage block with zeros.
        // Treat zero-only as "no real wire usage" so the estimator still runs.
        let body = json!({"usage": {"prompt_tokens": 0, "completion_tokens": 0}});
        assert!(!usage_present(&body));
    }

    #[test]
    fn estimate_input_walks_system_user_tools() {
        let req = json!({
            "messages": [
                {"role":"system","content":"You are helpful"},
                {"role":"user","content":"Hello"}
            ],
            "tools": [
                {"type":"function","function":{"name":"calc","description":"adds two numbers","parameters":{}}}
            ]
        });
        let n = estimate_input_tokens(&req, &cl());
        // Each piece independently is > 0; the sum must dominate the trivial
        // (3-token) overhead of any individual fragment.
        assert!(n > 5, "expected >5 tokens, got {n}");
    }

    #[test]
    fn estimate_input_handles_array_content() {
        let req = json!({
            "messages": [
                {"role":"user","content":[
                    {"type":"text","text":"part one"},
                    {"type":"text","text":"part two"}
                ]}
            ]
        });
        let n = estimate_input_tokens(&req, &cl());
        assert!(n >= 4, "expected >=4 tokens for two text parts, got {n}");
    }

    #[test]
    fn estimate_output_text_only() {
        let resp = json!({
            "choices": [{
                "index": 0,
                "message": {"role":"assistant","content":"Hello world"},
                "finish_reason":"stop"
            }]
        });
        let n = estimate_output_tokens(&resp, &cl());
        assert_eq!(n, 2); // "Hello world" is 2 cl100k tokens
    }

    #[test]
    fn estimate_output_includes_reasoning_content() {
        let resp = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role":"assistant",
                    "reasoning_content":"long internal trace abcdef",
                    "content":"final"
                },
                "finish_reason":"stop"
            }]
        });
        let with_reasoning = estimate_output_tokens(&resp, &cl());
        let resp_no_reasoning = json!({
            "choices": [{
                "index":0,
                "message":{"role":"assistant","content":"final"},
                "finish_reason":"stop"
            }]
        });
        let plain = estimate_output_tokens(&resp_no_reasoning, &cl());
        assert!(
            with_reasoning > plain,
            "reasoning_content must inflate output count (got {with_reasoning} vs plain {plain})"
        );
    }

    #[test]
    fn estimate_output_dedupes_reasoning_field_against_reasoning_content() {
        // Both fields carry SAME text. Counted once.
        let resp_dup = json!({
            "choices": [{
                "index":0,
                "message": {
                    "role":"assistant",
                    "reasoning_content":"trace text",
                    "reasoning":"trace text",
                    "content":"answer"
                },
                "finish_reason":"stop"
            }]
        });
        let resp_single = json!({
            "choices": [{
                "index":0,
                "message": {
                    "role":"assistant",
                    "reasoning_content":"trace text",
                    "content":"answer"
                },
                "finish_reason":"stop"
            }]
        });
        assert_eq!(
            estimate_output_tokens(&resp_dup, &cl()),
            estimate_output_tokens(&resp_single, &cl())
        );
    }

    #[test]
    fn estimate_output_extracts_think_blocks_from_content() {
        let resp = json!({
            "choices": [{
                "index":0,
                "message": {
                    "role":"assistant",
                    "content":"<think>plan ahead</think>final"
                },
                "finish_reason":"stop"
            }]
        });
        let n = estimate_output_tokens(&resp, &cl());
        let plain = estimate_output_tokens(
            &json!({"choices":[{"index":0,"message":{"role":"assistant","content":"final"},"finish_reason":"stop"}]}),
            &cl(),
        );
        assert!(
            n > plain,
            "<think> tokens must be counted (got {n} vs plain {plain})"
        );
    }

    #[test]
    fn estimate_output_includes_tool_calls() {
        let resp = json!({
            "choices": [{
                "index":0,
                "message": {
                    "role":"assistant",
                    "content": null,
                    "tool_calls":[{"id":"call_1","type":"function","function":{"name":"do","arguments":"{}"}}]
                },
                "finish_reason":"tool_calls"
            }]
        });
        let n = estimate_output_tokens(&resp, &cl());
        assert!(n > 0, "tool_calls must contribute tokens");
    }

    #[test]
    fn estimate_output_zero_on_unexpected_shape() {
        let resp = json!({"foo":"bar"});
        assert_eq!(estimate_output_tokens(&resp, &cl()), 0);
    }

    #[test]
    fn test_extract_sse_ignores_responses_events() {
        // Defensive: if a Responses event slipped into a Chat stream, we skip
        // it rather than misinterpret it. In practice the router prevents
        // this, but the parser should be self-defending.
        let events = vec![
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_x","model":"gpt-5","status":"completed"}}"#,
            ),
            make_sse(
                "",
                r#"{"id":"chatcmpl-1","model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(info.model.as_deref(), Some("gpt-4"));
        assert_eq!(info.finish_reason.as_deref(), Some("stop"));
    }

    // ─── first_user_text: strict "first role:user" semantics ────────────────

    #[test]
    fn first_user_text_returns_text_of_first_user_message() {
        let req = serde_json::json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "more"}
            ]
        });
        assert_eq!(first_user_text(&req).as_deref(), Some("hello"));
    }

    #[test]
    fn first_user_text_none_when_first_user_is_empty_string() {
        let req = serde_json::json!({
            "messages": [
                {"role": "user", "content": ""},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "later"}
            ]
        });
        assert_eq!(first_user_text(&req), None);
    }

    #[test]
    fn first_user_text_none_when_first_user_has_no_text_blocks() {
        // multimodal-only first user message (image_url with no text part).
        let req = serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:..."}}
                ]},
                {"role": "assistant", "content": "ack"},
                {"role": "user", "content": "later"}
            ]
        });
        assert_eq!(first_user_text(&req), None);
    }

    // ─── first_assistant_sig_from_request: strict "first only" semantics ────

    #[test]
    fn first_assistant_sig_from_request_returns_first_assistants_tool_call() {
        let req = serde_json::json!({
            "messages": [
                {"role":"system","content":"sys"},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_first","type":"function","function":{"name":"f","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_first","content":"ok"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_second","type":"function","function":{"name":"f","arguments":"{}"}}
                ]}
            ]
        });
        let sig = first_assistant_sig_from_request(&req).unwrap();
        assert!(matches!(sig, AssistantSig::ToolId(id) if id == "call_first"));
    }

    #[test]
    fn first_assistant_sig_from_request_none_when_first_assistant_is_empty() {
        // First assistant has null content and no tool_calls — pathological
        // shape (truncated/malformed). Must NOT fall through to the second
        // assistant message (which would be turn-2's anchor, not turn-1's).
        let req = serde_json::json!({
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":null},
                {"role":"user","content":"more"},
                {"role":"assistant","content":"this would be a leak"}
            ]
        });
        assert!(first_assistant_sig_from_request(&req).is_none());
    }

    #[test]
    fn first_assistant_sig_from_request_none_when_first_tool_call_lacks_id() {
        let req = serde_json::json!({
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"type":"function","function":{"name":"f","arguments":"{}"}}
                ]},
                {"role":"user","content":"more"},
                {"role":"assistant","content":"this would be a leak"}
            ]
        });
        assert!(first_assistant_sig_from_request(&req).is_none());
    }
}
