use std::collections::BTreeMap;

use serde_json::Value;

use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{RequestInfo, ResponseInfo, RouteVerdict, WireApi};
use crate::parsed_json::ParsedJson;

/// Check whether the request carries an Anthropic-style API key, either via
/// `x-api-key` (direct keys) or `Authorization: Bearer` (OAuth tokens issued
/// by Anthropic still aren't `sk-ant-*`, so this only catches direct keys
/// sent through either header).
fn has_anthropic_api_key(req: &HttpRequestData) -> bool {
    if req
        .header("x-api-key")
        .map(|v| v.starts_with("sk-ant-"))
        .unwrap_or(false)
    {
        return true;
    }
    req.header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.starts_with("sk-ant-"))
        .unwrap_or(false)
}

/// Wire-API implementation for Anthropic Messages API.
pub struct AnthropicWireApi;

impl WireApi for AnthropicWireApi {
    fn name(&self) -> &'static str {
        super::ANTHROPIC
    }

    fn classify_route(&self, req: &HttpRequestData) -> RouteVerdict {
        if req.method != "POST" {
            return RouteVerdict::Reject;
        }
        let path = req.uri.split('?').next().unwrap_or(&req.uri);

        // Auxiliary Anthropic endpoints are not inference calls. Reject
        // before the header-only accept rule below can catch them.
        if path.ends_with("/v1/messages/count_tokens") || path.ends_with("/v1/messages/batches") {
            return RouteVerdict::Reject;
        }

        let has_anthropic_version = req.header("anthropic-version").is_some();
        let has_anthropic_key = has_anthropic_api_key(req);

        // Canonical inference path.
        if path.ends_with("/v1/messages") && (has_anthropic_version || has_anthropic_key) {
            return RouteVerdict::Accept;
        }

        // Non-standard path (gateway prefix, tenant routing, ...) but the
        // request is unambiguously Anthropic by header.
        if has_anthropic_version {
            return RouteVerdict::Accept;
        }

        RouteVerdict::Unknown
    }

    fn matches_shape(&self, _req: &HttpRequestData, req_body: &ParsedJson) -> bool {
        let Some(body) = req_body.get() else {
            return false;
        };
        // Required spec fields: {model, messages, max_tokens}. Presence of
        // `messages` array and `model` string is table stakes.
        if body.get("model").and_then(|v| v.as_str()).is_none() {
            return false;
        }
        let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
            return false;
        };

        // Anthropic InputMessage.role is strictly {user, assistant}. Any
        // system/developer/tool role in `messages[]` proves it's OpenAI-shaped.
        let strict_roles = messages.iter().all(|m| {
            matches!(
                m.get("role").and_then(|r| r.as_str()),
                Some("user" | "assistant")
            )
        });
        if !strict_roles {
            return false;
        }

        // `input` is the Responses API discriminator — not ours.
        if body.get("input").is_some() {
            return false;
        }

        // Beyond the roles constraint, require at least one Anthropic-exclusive
        // signal. `max_tokens` alone is too weak (OpenAI accepts it too); we
        // use it only in combination with other signals via the top-level
        // `system` field or `stop_sequences` array which OpenAI Chat doesn't
        // have (OpenAI uses `stop` + inline `{"role":"system"}` messages).
        body.get("system").is_some() || body.get("stop_sequences").is_some()
    }

    fn extract_request(&self, req: &HttpRequestData, req_body: &ParsedJson) -> RequestInfo {
        extract_from_request(req, req_body)
    }
    fn extract_response(&self, resp: &HttpResponseData, resp_body: &ParsedJson) -> ResponseInfo {
        extract_from_response(resp, resp_body)
    }
    fn extract_sse(&self, events: &[SseEventData]) -> (ResponseInfo, ParsedJson) {
        extract_from_sse(events)
    }

    fn is_terminal(&self, finish_reason: &str) -> bool {
        matches!(
            finish_reason,
            "end_turn"
                | "stop_sequence"
                | "max_tokens"
                | "tool_use"
                | "refusal"
                | "model_context_window_exceeded"
        )
        // pause_turn intentionally absent — server-tool loop yielded mid-turn.
    }

    fn is_tool_use(&self, finish_reason: &str) -> bool {
        finish_reason == "tool_use"
    }
}

/// Extract request info from an Anthropic API request. Reads JSON via the
/// shared parse cache, which lazy-parses on first access.
pub fn extract_from_request(_req: &HttpRequestData, req_body: &ParsedJson) -> RequestInfo {
    let body = req_body.get();
    let model = body
        .and_then(|b| b.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let is_stream = body
        .and_then(|b| b.get("stream"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    RequestInfo { model, is_stream }
}

/// Extract response info from a non-streaming Anthropic response.
pub fn extract_from_response(resp: &HttpResponseData, resp_body: &ParsedJson) -> ResponseInfo {
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
        .and_then(|b| b.get("stop_reason"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let usage = parsed.and_then(|b| b.get("usage"));
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let cache_read_input_tokens = usage
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let cache_creation_input_tokens = usage
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens: None,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        response_body: body_str,
        response_id,
    }
}

/// Extract response info from accumulated SSE events (streaming response).
///
/// Per-event Values are parsed exactly once each. The synthetic body is
/// assembled as a `Value`, returned as a body-bound `ParsedJson` cache
/// for downstream readers, and only then serialized once to a `String`
/// for storage — never round-trips through serialize+parse.
pub fn extract_from_sse(sse_events: &[SseEventData]) -> (ResponseInfo, ParsedJson) {
    let mut model: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut cache_read_input_tokens: Option<u32> = None;
    let mut cache_creation_input_tokens: Option<u32> = None;
    let mut response_id: Option<String> = None;

    // Parse each event's `data` exactly once; reuse across the main loop and
    // the synthetic-body reconstruction below.
    let parsed: Vec<Value> = sse_events
        .iter()
        .map(|e| serde_json::from_str(&e.data).unwrap_or(Value::Null))
        .collect();

    for (event, data) in sse_events.iter().zip(parsed.iter()) {
        match event.event_type.as_str() {
            "message_start" => {
                // message_start contains the message object with model info.
                if let Some(msg) = data.get("message") {
                    if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                        response_id = Some(id.to_string());
                    }
                    if let Some(m) = msg.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                    // message_start may include initial usage (input_tokens + cache).
                    if let Some(usage) = msg.get("usage") {
                        if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                            input_tokens = Some(it as u32);
                        }
                        if let Some(cr) = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_input_tokens = Some(cr as u32);
                        }
                        if let Some(cc) = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            cache_creation_input_tokens = Some(cc as u32);
                        }
                    }
                }
            }
            "message_delta" => {
                // message_delta contains stop_reason and final usage.
                if let Some(delta) = data.get("delta") {
                    if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                        finish_reason = Some(sr.to_string());
                    }
                }
                if let Some(usage) = data.get("usage") {
                    if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        if it > 0 {
                            input_tokens = Some(it as u32);
                        }
                    }
                    if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        output_tokens = Some(ot as u32);
                    }
                    if let Some(cr) = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        cache_read_input_tokens = Some(cr as u32);
                    }
                    if let Some(cc) = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        cache_creation_input_tokens = Some(cc as u32);
                    }
                }
            }
            _ => {}
        }
    }

    // Build the synthetic response body as a Value; serialize once for
    // storage and hand the same Value to the returned response cache.
    let synthetic = build_response_body(sse_events, &parsed);
    let response_body = synthetic.as_ref().map(Value::to_string);
    let response_cache = ParsedJson::from_value(synthetic);

    let info = ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens: None,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        response_body,
        response_id,
    };
    (info, response_cache)
}

/// Per-block accumulation state, keyed by the `index` field carried on every
/// `content_block_*` event. Anthropic spec emits `index` on every delta and
/// stop, but parallel-tool-use streams (e.g. GLM-5) interleave the deltas
/// across indexes — keying by `index` is the only correct way to attribute
/// each `input_json_delta` / `text_delta` to its block.
struct PartialBlock {
    /// Initial `content_block` payload from `content_block_start` (carries
    /// `type`, `id`, `name`, etc.). Mutated in-place at finalize time.
    block: Value,
    /// `text_delta.text` or `thinking_delta.thinking` accumulator.
    text: String,
    /// `input_json_delta.partial_json` accumulator.
    json: String,
}

/// Assemble a synthetic response JSON from SSE content events.
///
/// Anthropic streaming emits content in `content_block_start` (block type/initial)
/// and `content_block_delta` (incremental text/json) events. We reconstruct the
/// final content blocks array keyed by the `index` field on every event, then
/// wrap in a message-like JSON object that includes model, usage, and
/// stop_reason from `message_start`/`message_delta`.
///
/// Takes events alongside their already-parsed JSON values so we don't re-parse
/// the same event bodies the caller (`extract_from_sse`) already parsed.
/// Returns a `Value`; the caller serializes once for storage and hands the
/// same `Value` to the per-call parse cache.
fn build_response_body(sse_events: &[SseEventData], parsed: &[Value]) -> Option<Value> {
    use serde_json::json;

    let mut message_obj: Value = Value::Null;
    let mut stop_reason: Option<String> = None;
    let mut final_usage: Option<Value> = None;

    // BTreeMap so end-of-stream drain emits blocks in index order. Same pattern
    // as the OpenAI Chat tool_calls accumulator
    // (`wire_apis/openai/chat.rs` -> `tool_calls: BTreeMap<u64, _>`).
    let mut blocks: BTreeMap<u64, PartialBlock> = BTreeMap::new();

    for (event, data) in sse_events.iter().zip(parsed.iter()) {
        match event.event_type.as_str() {
            "message_start" => {
                if let Some(msg) = data.get("message") {
                    message_obj = msg.clone();
                }
            }
            "content_block_start" => {
                let Some(idx) = data.get("index").and_then(|v| v.as_u64()) else {
                    continue;
                };
                if let Some(cb) = data.get("content_block") {
                    blocks.insert(
                        idx,
                        PartialBlock {
                            block: cb.clone(),
                            text: String::new(),
                            json: String::new(),
                        },
                    );
                }
            }
            "content_block_delta" => {
                let Some(idx) = data.get("index").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let Some(entry) = blocks.get_mut(&idx) else {
                    continue; // delta for an index we never saw a start for; drop
                };
                if let Some(delta) = data.get("delta") {
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                entry.text.push_str(t);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(j) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                entry.json.push_str(j);
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                                entry.text.push_str(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            // content_block_stop carries no payload we need beyond what start
            // and delta already supplied; per-index entries stay live until
            // the end-of-stream drain so a late delta with the same index
            // still lands.
            "content_block_stop" => {}
            "message_delta" => {
                if let Some(delta) = data.get("delta") {
                    if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                        stop_reason = Some(sr.to_string());
                    }
                }
                if let Some(usage) = data.get("usage") {
                    final_usage = Some(usage.clone());
                }
            }
            _ => {}
        }
    }

    // Drain in index order and finalize each block.
    let content_blocks: Vec<Value> = blocks.into_values().map(finalize_block).collect();

    if message_obj.is_null() && content_blocks.is_empty() {
        return None;
    }

    // Build the synthetic message object
    let mut result = if message_obj.is_object() {
        message_obj
    } else {
        json!({})
    };

    if let Some(obj) = result.as_object_mut() {
        obj.insert("content".to_string(), Value::Array(content_blocks));
        if let Some(sr) = stop_reason {
            obj.insert("stop_reason".to_string(), Value::String(sr));
        }
        if let Some(usage) = final_usage {
            obj.insert("usage".to_string(), usage);
        }
    }

    Some(result)
}

/// Move accumulated text/json from a `PartialBlock` into the final block JSON.
/// `tool_use` falls back to the raw json string when parsing fails so the
/// caller keeps the same observable behavior as the previous implementation
/// for malformed streams.
fn finalize_block(partial: PartialBlock) -> Value {
    let PartialBlock {
        mut block,
        text,
        json,
    } = partial;
    if let Some(obj) = block.as_object_mut() {
        let block_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                obj.insert("text".to_string(), Value::String(text));
            }
            "thinking" => {
                obj.insert("thinking".to_string(), Value::String(text));
            }
            "tool_use" => {
                let input = serde_json::from_str::<Value>(&json).unwrap_or(Value::String(json));
                obj.insert("input".to_string(), input);
            }
            _ => {}
        }
    }
    block
}

// ────────────────────────── Body shape parsers ─────────────────────────────
//
// Helpers that walk Anthropic Messages request / response bodies to pull
// out the pieces agent profiles need (first/last user text, assistant
// signature, tool names, system prompt). All operate on a parsed
// `serde_json::Value` (request side) or raw `&str` (response side, where
// the caller doesn't already have a parsed value handy).
//
// These are wire-api-shape concerns, shared by every profile that
// classifies Anthropic traffic — `openclaw` and `generic` today.

use super::AssistantSig;

/// `Some(vec)` when `tools` is absent (treated as empty) or is an array
/// (extracts each `.name`). `None` only when `tools` is present but has a
/// wrong shape (e.g., scalar) — i.e. the body is genuinely unparseable.
/// Treating absent as empty matches profiles whose marker-detection paths
/// expect "no tools" to mean an empty list, not a parse failure.
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

/// Anthropic uses a top-level `system` field. Accepts both the string
/// shorthand and the `[{"type":"text", "text":"..."}]` array form.
pub fn first_system_text(req: &Value) -> Option<String> {
    match req.get("system")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .next()
            .map(str::to_string),
        _ => None,
    }
}

/// Extract text from the **first** `role:user` message — and only that one.
/// If the first user message has no extractable text (empty string, image-
/// only blocks, tool_result-only blocks), return `None` rather than falling
/// through to later user messages. Falling through could pick up text from
/// turn 2+, which is not a stable session anchor.
pub fn first_user_text(req: &Value) -> Option<String> {
    let msgs = req.get("messages")?.as_array()?;
    let first_user = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    match first_user.get("content")? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
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

/// Sig of the **first** `role:assistant` message — and only that one. If
/// the first assistant has no `tool_use` block AND no `text` block, return
/// `None` rather than falling through to later assistant messages (which
/// would belong to turn 2+ and are not a stable session anchor).
/// Within that one message, `tool_use` takes precedence over `text`.
pub fn first_assistant_sig_from_request(req: &Value) -> Option<AssistantSig> {
    let msgs = req.get("messages")?.as_array()?;
    let first_assistant = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))?;
    let blocks = first_assistant.get("content")?.as_array()?;
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            if let Some(id) = b.get("id").and_then(|x| x.as_str()) {
                return Some(AssistantSig::ToolId(id.to_string()));
            }
        }
    }
    let parts: Vec<String> = blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(AssistantSig::Text(parts.join("\n")))
    }
}

pub fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let resp: Value = serde_json::from_str(body).ok()?;
    first_assistant_sig_from_response_value(&resp)
}

/// `Value`-input sibling of `first_assistant_sig_from_response`. Used by
/// agent-profile session-id extractors on the parse-once hot path.
pub fn first_assistant_sig_from_response_value(resp: &Value) -> Option<AssistantSig> {
    let blocks = resp.get("content")?.as_array()?;
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            if let Some(id) = b.get("id").and_then(|x| x.as_str()) {
                return Some(AssistantSig::ToolId(id.to_string()));
            }
        }
    }
    let parts: Vec<String> = blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(AssistantSig::Text(parts.join("\n")))
    }
}

/// Extract the trimmed text of the **last** user message. `None` when the
/// last message isn't role=user, content has no text, or text is
/// whitespace-only. Equivalent to `AgentProfile::extract_user_input` for
/// Anthropic-shape bodies.
pub fn extract_user_input(req: &Value) -> Option<String> {
    let last = req.get("messages")?.as_array()?.last()?;
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return None;
    }
    let raw = match last.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .collect();
            parts.join("\n")
        }
        _ => return None,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// True iff the **last** message is role=user AND contains at least one
/// non-`tool_result` block with non-empty content (i.e. a fresh user-side
/// turn rather than a tool roundtrip continuation). Equivalent to
/// `AgentProfile::is_user_turn_start` for Anthropic-shape bodies.
pub fn is_user_turn_start(req: &Value) -> Option<bool> {
    let last = req.get("messages")?.as_array()?.last()?;
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return Some(false);
    }
    match last.get("content")? {
        Value::String(s) => Some(!s.trim().is_empty()),
        Value::Array(blocks) => Some(blocks.iter().any(|b| {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("tool_result") => false,
                Some("text") => b
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false),
                Some(_) => true, // image, future block types — count as user-visible
                None => false,
            }
        })),
        _ => None,
    }
}

/// Concatenated text of the assistant's final response. `None` when body
/// is unparseable, the response has no `content` blocks, or all text
/// blocks are empty/whitespace. Equivalent to
/// `AgentProfile::extract_assistant_text` for Anthropic-shape responses.
pub fn extract_assistant_text(body: &str) -> Option<String> {
    let resp: Value = serde_json::from_str(body).ok()?;
    extract_assistant_text_value(&resp)
}

/// `Value`-input sibling of `extract_assistant_text`. Used by agent-profile
/// extractors on the parse-once hot path.
pub fn extract_assistant_text_value(resp: &Value) -> Option<String> {
    let blocks = resp.get("content")?.as_array()?;
    let parts: Vec<String> = blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .collect();
    let joined = parts.join("\n");
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

// ─────────────────────────── Token estimation ─────────────────────────────

use crate::token_estimator::{collect_anthropic_assistant_text, TokenEstimator};

/// True iff the response body carried any Anthropic `usage` field with a
/// non-zero count.
pub fn usage_present(body: &Value) -> bool {
    let u = match body.get("usage") {
        Some(v) => v,
        None => return false,
    };
    if !u.is_object() {
        return false;
    }
    u.get("input_tokens")
        .and_then(|v| v.as_u64())
        .map(|n| n > 0)
        .unwrap_or(false)
        || u.get("output_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n > 0)
            .unwrap_or(false)
}

/// Walk an assistant-shaped Anthropic body (top-level `content[*]` blocks)
/// and produce one concatenated, deduped string of all reasoning + text +
/// serialized tool_use blocks.
pub fn collected_response_text(body: &Value) -> String {
    collect_anthropic_assistant_text(body)
}

/// Estimate input tokens for an Anthropic request. Walks `system` (string
/// or `[{type:"text",text:...}]`), `messages[*]` (with role + content array
/// or string), and `tools[*]` (each schema serialized whole).
pub fn estimate_input_tokens(req: &Value, est: &dyn TokenEstimator) -> u32 {
    let mut total: u32 = 0;

    // system: string OR array of {type:"text",text:...}
    if let Some(s) = req.get("system").and_then(|v| v.as_str()) {
        total = total.saturating_add(est.count_text(s));
    } else if let Some(arr) = req.get("system").and_then(|v| v.as_array()) {
        for blk in arr {
            if let Some(t) = blk.get("text").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(t));
            }
        }
    }

    if let Some(msgs) = req.get("messages").and_then(|v| v.as_array()) {
        for m in msgs {
            if let Some(role) = m.get("role").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(role));
            }
            if let Some(s) = m.get("content").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(s));
            } else if let Some(arr) = m.get("content").and_then(|v| v.as_array()) {
                for blk in arr {
                    let kind = blk.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match kind {
                        "text" => {
                            if let Some(t) = blk.get("text").and_then(|v| v.as_str()) {
                                total = total.saturating_add(est.count_text(t));
                            }
                        }
                        "thinking" => {
                            if let Some(t) = blk.get("thinking").and_then(|v| v.as_str()) {
                                total = total.saturating_add(est.count_text(t));
                            }
                        }
                        "tool_use" | "tool_result" => {
                            if let Ok(s) = serde_json::to_string(blk) {
                                total = total.saturating_add(est.count_text(&s));
                            }
                        }
                        _ => {
                            // image / other modal blocks: skip — we don't tokenize binary.
                        }
                    }
                }
            }
        }
    }

    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            if let Ok(s) = serde_json::to_string(t) {
                total = total.saturating_add(est.count_text(&s));
            }
        }
    }
    if let Some(tc) = req.get("tool_choice") {
        if let Ok(s) = serde_json::to_string(tc) {
            total = total.saturating_add(est.count_text(&s));
        }
    }

    total
}

/// Estimate output tokens for an Anthropic response body. Counts thinking +
/// text + tool_use blocks; deduplicates think-block text against thinking
/// blocks.
pub fn estimate_output_tokens(resp: &Value, est: &dyn TokenEstimator) -> u32 {
    let text = collected_response_text(resp);
    est.count_text(&text)
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

    fn parse_events(events: &[SseEventData]) -> Vec<Value> {
        events
            .iter()
            .map(|e| serde_json::from_str(&e.data).unwrap_or(Value::Null))
            .collect()
    }

    #[test]
    fn test_build_response_body_single_text_block() {
        let events = vec![
            make_sse(
                "message_start",
                r#"{"message":{"id":"msg_01","model":"claude-3","role":"assistant","usage":{"input_tokens":10}}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse(
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            ),
        ];
        let parsed = parse_events(&events);
        let v = build_response_body(&events, &parsed).unwrap();
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "Hello world");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["model"], "claude-3");
    }

    #[test]
    fn test_build_response_body_thinking_block() {
        let events = vec![
            make_sse(
                "message_start",
                r#"{"message":{"id":"msg_02","model":"claude-3","role":"assistant"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse(
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"text_delta","text":"Answer"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":1}"#),
            make_sse("message_delta", r#"{"delta":{"stop_reason":"end_turn"}}"#),
        ];
        let parsed = parse_events(&events);
        let v = build_response_body(&events, &parsed).unwrap();
        // thinking block uses "thinking" field, not "text"
        assert_eq!(v["content"][0]["type"], "thinking");
        assert_eq!(v["content"][0]["thinking"], "Let me think...");
        assert!(v["content"][0].get("text").is_none());
        // text block uses "text" field
        assert_eq!(v["content"][1]["type"], "text");
        assert_eq!(v["content"][1]["text"], "Answer");
    }

    #[test]
    fn test_build_response_body_tool_use_block() {
        let events = vec![
            make_sse(
                "message_start",
                r#"{"message":{"id":"msg_03","model":"claude-3","role":"assistant"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"tool_use","id":"tu_01","name":"read_file"}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"\"foo.txt\"}"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse("message_delta", r#"{"delta":{"stop_reason":"tool_use"}}"#),
        ];
        let parsed = parse_events(&events);
        let v = build_response_body(&events, &parsed).unwrap();
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["input"]["path"], "foo.txt");
        assert_eq!(v["stop_reason"], "tool_use");
    }

    /// Reproduces the GLM-5 / openclaw pattern: parallel `tool_use` blocks
    /// where all `content_block_start` events arrive before any deltas, and
    /// per-index deltas/stops are interleaved out of natural order.
    /// Pre-fix this test failed: index=1's `tool_use` ended up with `input:""`
    /// and the `ps aux` command was silently dropped.
    #[test]
    fn test_build_response_body_interleaved_parallel_tool_use() {
        let events = vec![
            make_sse(
                "message_start",
                r#"{"message":{"id":"msg_glm","model":"glm-5","role":"assistant"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            // Both tool_use starts arrive before any delta — this is the
            // shape that GLM emits and that the pre-fix accumulator
            // mishandled.
            make_sse(
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"tool_use","id":"call_aaa","name":"exec"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":2,"content_block":{"type":"tool_use","id":"call_bbb","name":"exec"}}"#,
            ),
            // index=2 finalizes first.
            make_sse(
                "content_block_delta",
                r#"{"index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"systemctl status\"}"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":2}"#),
            // index=1's deltas arrive after index=2 already stopped.
            make_sse(
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ps aux\"}"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":1}"#),
            make_sse("message_delta", r#"{"delta":{"stop_reason":"tool_use"}}"#),
        ];
        let parsed = parse_events(&events);
        let v = build_response_body(&events, &parsed).unwrap();
        // Blocks are emitted in index order regardless of stream interleaving.
        assert_eq!(v["content"][0]["type"], "thinking");
        assert_eq!(v["content"][0]["thinking"], "plan");
        assert_eq!(v["content"][1]["type"], "tool_use");
        assert_eq!(v["content"][1]["id"], "call_aaa");
        assert_eq!(v["content"][1]["input"]["command"], "ps aux");
        assert_eq!(v["content"][2]["type"], "tool_use");
        assert_eq!(v["content"][2]["id"], "call_bbb");
        assert_eq!(v["content"][2]["input"]["command"], "systemctl status");
    }

    /// Three parallel tool_use blocks — second openclaw response shape: all
    /// starts first, then per-index delta+stop in order. Pre-fix two of the
    /// three blocks ended up with `input:""` and the third inherited the
    /// wrong block's command.
    #[test]
    fn test_build_response_body_three_parallel_tools_starts_first() {
        let events = vec![
            make_sse(
                "message_start",
                r#"{"message":{"id":"msg_3p","model":"glm-5","role":"assistant"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"tool_use","id":"call_1","name":"exec"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"tool_use","id":"call_2","name":"exec"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":2,"content_block":{"type":"tool_use","id":"call_3","name":"exec"}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"tail\"}"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse(
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ss\"}"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":1}"#),
            make_sse(
                "content_block_delta",
                r#"{"index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"tmux\"}"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":2}"#),
            make_sse("message_delta", r#"{"delta":{"stop_reason":"tool_use"}}"#),
        ];
        let parsed = parse_events(&events);
        let v = build_response_body(&events, &parsed).unwrap();
        assert_eq!(v["content"][0]["id"], "call_1");
        assert_eq!(v["content"][0]["input"]["command"], "tail");
        assert_eq!(v["content"][1]["id"], "call_2");
        assert_eq!(v["content"][1]["input"]["command"], "ss");
        assert_eq!(v["content"][2]["id"], "call_3");
        assert_eq!(v["content"][2]["input"]["command"], "tmux");
    }

    /// A delta whose `index` was never opened by a `content_block_start` is
    /// dropped silently and does not contaminate any other block.
    #[test]
    fn test_build_response_body_delta_for_unknown_index_dropped() {
        let events = vec![
            make_sse(
                "message_start",
                r#"{"message":{"id":"msg_orph","model":"claude-3","role":"assistant"}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            // Orphaned delta — never had a matching start.
            make_sse(
                "content_block_delta",
                r#"{"index":99,"delta":{"type":"text_delta","text":"GHOST"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
        ];
        let parsed = parse_events(&events);
        let v = build_response_body(&events, &parsed).unwrap();
        assert_eq!(v["content"].as_array().unwrap().len(), 1);
        assert_eq!(v["content"][0]["text"], "Hello");
    }

    #[test]
    fn test_build_response_body_empty_stream() {
        let events: Vec<SseEventData> = vec![];
        let parsed: Vec<Value> = vec![];
        assert!(build_response_body(&events, &parsed).is_none());
    }

    #[test]
    fn test_extract_response_id_non_streaming() {
        let ip: IpAddr = IpAddr::from([127, 0, 0, 1]);
        let body = serde_json::json!({
            "id": "msg_01XFDUDYJgAACzvnptvVoYEL",
            "type": "message",
            "model": "claude-3-opus-20240229",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 25, "output_tokens": 10}
        });
        let resp = ts_protocol::model::HttpResponseData {
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
        let info = extract_from_response(&resp, &cache);
        assert_eq!(
            info.response_id.as_deref(),
            Some("msg_01XFDUDYJgAACzvnptvVoYEL")
        );
    }

    #[test]
    fn predicates_anthropic() {
        let w = AnthropicWireApi;
        assert!(w.is_terminal("end_turn"));
        assert!(w.is_terminal("max_tokens"));
        assert!(w.is_terminal("refusal"));
        assert!(w.is_terminal("model_context_window_exceeded"));
        assert!(!w.is_terminal("pause_turn"));
        assert!(!w.is_terminal("unknown_future_value"));
        assert!(w.is_tool_use("tool_use"));
        assert!(!w.is_tool_use("end_turn"));
    }

    #[test]
    fn test_extract_response_id_streaming() {
        let events = vec![
            make_sse(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_stream_01","model":"claude-3","role":"assistant","usage":{"input_tokens":10}}}"#,
            ),
            make_sse(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse(
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            ),
        ];
        let (info, _body) = extract_from_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("msg_stream_01"));
    }
}

#[cfg(test)]
mod estimator_tests {
    use super::*;
    use crate::token_estimator::CL100kEstimator;
    use serde_json::json;

    fn cl() -> CL100kEstimator {
        CL100kEstimator::new()
    }

    #[test]
    fn usage_present_true_when_set() {
        let body = json!({"usage": {"input_tokens": 10, "output_tokens": 5}});
        assert!(usage_present(&body));
    }

    #[test]
    fn usage_present_false_when_missing_or_zero() {
        assert!(!usage_present(&json!({})));
        assert!(!usage_present(
            &json!({"usage": {"input_tokens": 0, "output_tokens": 0}})
        ));
    }

    #[test]
    fn estimate_input_walks_string_system_messages_tools() {
        let req = json!({
            "system": "You are helpful",
            "messages": [
                {"role":"user","content":"Hello"}
            ],
            "tools": [
                {"name":"calc","description":"adds","input_schema":{"type":"object"}}
            ]
        });
        let n = estimate_input_tokens(&req, &cl());
        assert!(n > 5);
    }

    #[test]
    fn estimate_input_walks_array_system() {
        let req_arr = json!({
            "system":[{"type":"text","text":"You are helpful"}],
            "messages":[{"role":"user","content":"Hi"}]
        });
        let req_str = json!({
            "system":"You are helpful",
            "messages":[{"role":"user","content":"Hi"}]
        });
        // Both forms must yield the same input estimate.
        assert_eq!(
            estimate_input_tokens(&req_arr, &cl()),
            estimate_input_tokens(&req_str, &cl())
        );
    }

    #[test]
    fn estimate_input_walks_message_array_content() {
        let req = json!({
            "system":"sys",
            "messages":[
                {"role":"user","content":[
                    {"type":"text","text":"hello there"},
                    {"type":"tool_result","tool_use_id":"toolu_x","content":"ok"}
                ]}
            ]
        });
        let n = estimate_input_tokens(&req, &cl());
        assert!(n > 3);
    }

    #[test]
    fn estimate_output_thinking_plus_text_counted() {
        let resp = json!({
            "content":[
                {"type":"thinking","thinking":"plan ahead"},
                {"type":"text","text":"final answer"}
            ]
        });
        let n = estimate_output_tokens(&resp, &cl());
        let plain = estimate_output_tokens(
            &json!({"content":[{"type":"text","text":"final answer"}]}),
            &cl(),
        );
        assert!(
            n > plain,
            "thinking block must count (got {n} vs plain {plain})"
        );
    }

    #[test]
    fn estimate_output_dedupes_thinking_against_inline_think_block() {
        let resp = json!({
            "content":[
                {"type":"thinking","thinking":"trace"},
                {"type":"text","text":"<think>trace</think>answer"}
            ]
        });
        let n = estimate_output_tokens(&resp, &cl());
        // Compare against a response that omits the duplicate inline block.
        let single = json!({
            "content":[
                {"type":"thinking","thinking":"trace"},
                {"type":"text","text":"answer"}
            ]
        });
        assert_eq!(n, estimate_output_tokens(&single, &cl()));
    }

    #[test]
    fn estimate_output_includes_tool_use() {
        let resp = json!({
            "content":[
                {"type":"tool_use","id":"toolu_x","name":"calc","input":{"a":1}}
            ]
        });
        assert!(estimate_output_tokens(&resp, &cl()) > 0);
    }

    // ─── first_user_text: strict "first role:user" semantics ────────────────

    #[test]
    fn first_user_text_returns_text_of_first_user_message() {
        let req = json!({
            "messages": [
                {"role":"user","content":"hello"},
                {"role":"assistant","content":[{"type":"text","text":"hi"}]},
                {"role":"user","content":[{"type":"text","text":"more"}]}
            ]
        });
        assert_eq!(first_user_text(&req).as_deref(), Some("hello"));
    }

    #[test]
    fn first_user_text_none_when_first_user_is_empty_string() {
        // First user has empty content; second user has text. Strict
        // semantics: don't fall through — return None.
        let req = json!({
            "messages": [
                {"role":"user","content":""},
                {"role":"assistant","content":[{"type":"text","text":"hi"}]},
                {"role":"user","content":[{"type":"text","text":"later"}]}
            ]
        });
        assert_eq!(first_user_text(&req), None);
    }

    #[test]
    fn first_user_text_none_when_first_user_has_only_tool_result_blocks() {
        // Pathological: messages[0] is a tool_result-only user message
        // (would never happen in a real first turn, but covers the
        // fall-through behavior). Must return None — must not jump to
        // messages[2].
        let req = json!({
            "messages": [
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t","content":"ok"}
                ]},
                {"role":"assistant","content":[{"type":"text","text":"reply"}]},
                {"role":"user","content":[{"type":"text","text":"later"}]}
            ]
        });
        assert_eq!(first_user_text(&req), None);
    }

    // ─── first_assistant_sig_from_request: strict "first only" semantics ────

    #[test]
    fn first_assistant_sig_from_request_returns_first_assistants_tool_use() {
        let req = json!({
            "messages": [
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[
                    {"type":"text","text":"working"},
                    {"type":"tool_use","id":"toolu_first","name":"R","input":{}}
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_first","content":"ok"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_second","name":"R","input":{}}]}
            ]
        });
        let sig = first_assistant_sig_from_request(&req).unwrap();
        assert!(matches!(sig, AssistantSig::ToolId(id) if id == "toolu_first"));
    }

    #[test]
    fn first_assistant_sig_from_request_none_when_first_assistant_has_no_extractable_blocks() {
        // First assistant's content has neither `tool_use` nor `text` blocks
        // (e.g., thinking-only or filtered-out blocks). Must return None —
        // must not skip to the second assistant message.
        let req = json!({
            "messages": [
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"hidden chain-of-thought"}
                ]},
                {"role":"user","content":[{"type":"text","text":"more"}]},
                {"role":"assistant","content":[{"type":"text","text":"this would be a leak"}]}
            ]
        });
        assert!(first_assistant_sig_from_request(&req).is_none());
    }

    #[test]
    fn first_assistant_sig_from_request_none_when_first_assistant_content_empty_array() {
        let req = json!({
            "messages": [
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[]},
                {"role":"user","content":[{"type":"text","text":"more"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_leak","name":"R","input":{}}]}
            ]
        });
        assert!(first_assistant_sig_from_request(&req).is_none());
    }
}
