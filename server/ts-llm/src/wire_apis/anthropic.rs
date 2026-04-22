use serde_json::Value;

use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{FinishReason, RequestInfo, ResponseInfo, RouteVerdict, WireApi};

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

    fn matches_shape(&self, _req: &HttpRequestData, body: &Value) -> bool {
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

    fn extract_request(&self, req: &HttpRequestData) -> RequestInfo {
        extract_from_request(req)
    }
    fn extract_response(&self, resp: &HttpResponseData) -> ResponseInfo {
        extract_from_response(resp)
    }
    fn extract_sse(&self, events: &[SseEventData]) -> ResponseInfo {
        extract_from_sse(events)
    }

    fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
        use crate::model::{
            ParsedContentBlock, ParsedInput, ParsedMessage, ParsedRole, ParsedSampling,
            ParsedToolDef, ParsedToolResult,
        };
        let mut out = ParsedInput::default();

        // system (top-level string)
        if let Some(s) = body.get("system").and_then(|v| v.as_str()) {
            out.system = Some(s.to_string());
        }

        // tools
        if let Some(arr) = body.get("tools").and_then(|v| v.as_array()) {
            for t in arr {
                let name = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let description = t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let input_schema_json = t
                    .get("input_schema")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                out.tools.push(ParsedToolDef {
                    name,
                    description,
                    input_schema_json,
                });
            }
        }

        // sampling
        out.sampling = ParsedSampling {
            temperature: body.get("temperature").and_then(|v| v.as_f64()),
            max_tokens: body
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            top_p: body.get("top_p").and_then(|v| v.as_f64()),
            top_k: body.get("top_k").and_then(|v| v.as_u64()).map(|v| v as u32),
            stream: body.get("stream").and_then(|v| v.as_bool()),
            tool_choice: body.get("tool_choice").map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(v).unwrap_or_default(),
            }),
            stop: body
                .get("stop_sequences")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            response_format: None, // Anthropic doesn't have this.
        };

        // messages
        let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
            return out;
        };
        for msg in messages {
            let wire_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let mut role = match wire_role {
                "user" => ParsedRole::User,
                "assistant" => ParsedRole::Assistant,
                _ => continue, // Anthropic only allows user / assistant at the message level.
            };

            let mut blocks: Vec<ParsedContentBlock> = Vec::new();
            let content = msg.get("content");

            // Content can be a string OR an array of blocks.
            if let Some(s) = content.and_then(|v| v.as_str()) {
                blocks.push(ParsedContentBlock::Text {
                    text: s.to_string(),
                });
                // Maintain the legacy user_message field for the turn joiner.
                if wire_role == "user" {
                    out.user_message = Some(s.to_string());
                }
            } else if let Some(arr) = content.and_then(|v| v.as_array()) {
                let mut user_text_buf = String::new();
                for block in arr {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            let text = block
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if wire_role == "user" {
                                if !user_text_buf.is_empty() {
                                    user_text_buf.push('\n');
                                }
                                user_text_buf.push_str(&text);
                            }
                            blocks.push(ParsedContentBlock::Text { text });
                        }
                        Some("tool_use") => {
                            let id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args_json = block
                                .get("input")
                                .map(|v| serde_json::to_string(v).unwrap_or_default())
                                .unwrap_or_default();
                            blocks.push(ParsedContentBlock::ToolUse {
                                id,
                                name,
                                args_json,
                            });
                        }
                        Some("tool_result") => {
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let is_error = block
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let content_str = match block.get("content") {
                                Some(c) if c.is_string() => c.as_str().unwrap().to_string(),
                                Some(c) if c.is_array() => c
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                Some(c) => serde_json::to_string(c).unwrap_or_default(),
                                None => String::new(),
                            };
                            // Legacy turn-joiner field.
                            out.tool_results.push(ParsedToolResult {
                                tool_use_id: tool_use_id.clone(),
                                content: content_str.clone(),
                                is_error,
                            });
                            blocks.push(ParsedContentBlock::ToolResult {
                                tool_use_id,
                                content: content_str,
                                is_error,
                            });
                        }
                        Some("image") => {
                            let mime = block
                                .get("source")
                                .and_then(|s| s.get("media_type"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            blocks.push(ParsedContentBlock::Image {
                                mime,
                                size_bytes: None,
                            });
                        }
                        _ => {
                            blocks.push(ParsedContentBlock::Unknown(block.clone()));
                        }
                    }
                }
                if wire_role == "user" && !user_text_buf.is_empty() {
                    out.user_message = Some(user_text_buf);
                }
            }

            // Re-tag user messages whose content is exclusively tool_result blocks.
            if role == ParsedRole::User
                && !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|b| matches!(b, ParsedContentBlock::ToolResult { .. }))
            {
                role = ParsedRole::Tool;
            }

            out.messages.push(ParsedMessage {
                role,
                content: blocks,
            });
        }

        out
    }

    fn parse_output(&self, body: &Value) -> crate::model::ParsedOutput {
        use crate::model::{ParsedOutput, ParsedToolCall};
        let mut out = ParsedOutput::default();
        let Some(content) = body.get("content").and_then(|v| v.as_array()) else {
            return out;
        };
        let mut reasoning_buf = String::new();
        let mut message_buf = String::new();
        for block in content {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                        if !reasoning_buf.is_empty() {
                            reasoning_buf.push('\n');
                        }
                        reasoning_buf.push_str(t);
                    }
                }
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        if !message_buf.is_empty() {
                            message_buf.push('\n');
                        }
                        message_buf.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_json = block
                        .get("input")
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .unwrap_or_default();
                    out.tool_calls.push(ParsedToolCall {
                        id,
                        name,
                        args_json,
                    });
                }
                _ => {}
            }
        }
        if !reasoning_buf.is_empty() {
            out.reasoning = Some(reasoning_buf);
        }
        if !message_buf.is_empty() {
            out.message = Some(message_buf);
        }
        out
    }
}

/// Extract request info from an Anthropic API request.
pub fn extract_from_request(req: &HttpRequestData) -> RequestInfo {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Tenant ID: prefix of the API key (first 10 chars) for tenant differentiation.
    // Not a cryptographic hash — sufficient for grouping, not for security.
    // Anthropic accepts either `x-api-key` (direct API keys) or `Authorization: Bearer`
    // (OAuth tokens, e.g. Claude Code); check both.
    let tenant_id = req
        .header("x-api-key")
        .map(|key| key.to_string())
        .or_else(|| {
            req.header("authorization")
                .map(|auth| auth.strip_prefix("Bearer ").unwrap_or(auth).to_string())
        })
        .map(|token| {
            let prefix = if token.len() > 10 {
                &token[..10]
            } else {
                &token
            };
            prefix.to_string()
        });

    RequestInfo {
        model,
        is_stream,
        tenant_id,
    }
}

/// Extract response info from a non-streaming Anthropic response.
pub fn extract_from_response(resp: &HttpResponseData) -> ResponseInfo {
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
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .map(map_stop_reason);

    let input_tokens = body
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let output_tokens = body
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let cache_read_input_tokens = body
        .get("usage")
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let cache_creation_input_tokens = body
        .get("usage")
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
pub fn extract_from_sse(sse_events: &[SseEventData]) -> ResponseInfo {
    let mut model: Option<String> = None;
    let mut finish_reason: Option<FinishReason> = None;
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut cache_read_input_tokens: Option<u32> = None;
    let mut cache_creation_input_tokens: Option<u32> = None;
    let mut response_id: Option<String> = None;

    for event in sse_events {
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);

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
                        finish_reason = Some(map_stop_reason(sr));
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

    // Build a synthetic response body by assembling content from SSE events.
    let response_body = build_response_body(sse_events);

    ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens: None,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        response_body,
        response_id,
    }
}

/// Assemble a synthetic response JSON from SSE content events.
///
/// Anthropic streaming emits content in `content_block_start` (block type/initial)
/// and `content_block_delta` (incremental text/json) events. We reconstruct the
/// final content blocks array, then wrap in a message-like JSON object that
/// includes model, usage, and stop_reason from `message_start`/`message_delta`.
fn build_response_body(sse_events: &[SseEventData]) -> Option<String> {
    use serde_json::json;

    let mut message_obj: Value = Value::Null;
    let mut stop_reason: Option<String> = None;
    let mut final_usage: Option<Value> = None;

    // content_blocks: Vec<(type, accumulated_text_or_json)>
    let mut content_blocks: Vec<Value> = Vec::new();
    // Buffer for the current block being built via deltas
    let mut current_block: Option<Value> = None;
    let mut current_text = String::new();
    let mut current_json = String::new();

    for event in sse_events {
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);

        match event.event_type.as_str() {
            "message_start" => {
                if let Some(msg) = data.get("message") {
                    message_obj = msg.clone();
                }
            }
            "content_block_start" => {
                // Flush previous block if any
                flush_block(
                    &mut content_blocks,
                    &mut current_block,
                    &mut current_text,
                    &mut current_json,
                );
                if let Some(cb) = data.get("content_block") {
                    current_block = Some(cb.clone());
                    current_text.clear();
                    current_json.clear();
                }
            }
            "content_block_delta" => {
                if let Some(delta) = data.get("delta") {
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                                current_text.push_str(t);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(j) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                current_json.push_str(j);
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                                current_text.push_str(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                flush_block(
                    &mut content_blocks,
                    &mut current_block,
                    &mut current_text,
                    &mut current_json,
                );
            }
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

    // Flush any remaining block
    flush_block(
        &mut content_blocks,
        &mut current_block,
        &mut current_text,
        &mut current_json,
    );

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

    Some(result.to_string())
}

/// Flush the current content block into the blocks list,
/// filling in accumulated text or input JSON.
fn flush_block(
    blocks: &mut Vec<Value>,
    current_block: &mut Option<Value>,
    text_buf: &mut String,
    json_buf: &mut String,
) {
    if let Some(mut block) = current_block.take() {
        if let Some(obj) = block.as_object_mut() {
            let block_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match block_type {
                "text" => {
                    obj.insert("text".to_string(), Value::String(std::mem::take(text_buf)));
                }
                "thinking" => {
                    obj.insert(
                        "thinking".to_string(),
                        Value::String(std::mem::take(text_buf)),
                    );
                }
                "tool_use" => {
                    // Try to parse the accumulated JSON string into a Value
                    let input = serde_json::from_str::<Value>(json_buf)
                        .unwrap_or(Value::String(std::mem::take(json_buf)));
                    obj.insert("input".to_string(), input);
                    json_buf.clear();
                }
                _ => {}
            }
        }
        text_buf.clear();
        json_buf.clear();
        blocks.push(block);
    }
}

/// Map Anthropic stop_reason to normalized FinishReason.
fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" => FinishReason::Complete,
        "stop_sequence" => FinishReason::Complete,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolUse,
        _ => FinishReason::Error,
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
    fn test_map_stop_reason() {
        assert_eq!(map_stop_reason("end_turn"), FinishReason::Complete);
        assert_eq!(map_stop_reason("tool_use"), FinishReason::ToolUse);
        assert_eq!(map_stop_reason("max_tokens"), FinishReason::Length);
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
                r#"{"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":" world"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse(
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            ),
        ];
        let body = build_response_body(&events).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
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
                r#"{"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse(
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"Answer"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":1}"#),
            make_sse("message_delta", r#"{"delta":{"stop_reason":"end_turn"}}"#),
        ];
        let body = build_response_body(&events).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
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
                r#"{"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            ),
            make_sse(
                "content_block_delta",
                r#"{"delta":{"type":"input_json_delta","partial_json":"\"foo.txt\"}"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse("message_delta", r#"{"delta":{"stop_reason":"tool_use"}}"#),
        ];
        let body = build_response_body(&events).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["input"]["path"], "foo.txt");
        assert_eq!(v["stop_reason"], "tool_use");
    }

    #[test]
    fn test_build_response_body_empty_stream() {
        let events: Vec<SseEventData> = vec![];
        assert!(build_response_body(&events).is_none());
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
        let info = extract_from_response(&resp);
        assert_eq!(
            info.response_id.as_deref(),
            Some("msg_01XFDUDYJgAACzvnptvVoYEL")
        );
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
                r#"{"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            make_sse("content_block_stop", r#"{"index":0}"#),
            make_sse(
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            ),
        ];
        let info = extract_from_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("msg_stream_01"));
    }

    #[test]
    fn parse_output_text_only() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anthropic_output_text_only.json"
        ))
        .unwrap();
        let out = AnthropicWireApi.parse_output(&body);
        assert_eq!(out.reasoning, None);
        assert!(out
            .message
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        assert!(out.tool_calls.is_empty());
    }

    #[test]
    fn parse_output_tool_use() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anthropic_output_tool_use.json"
        ))
        .unwrap();
        let out = AnthropicWireApi.parse_output(&body);
        assert_eq!(out.tool_calls.len(), 1);
        let tc = &out.tool_calls[0];
        assert!(tc.id.starts_with("toolu_"));
        assert_eq!(tc.name, "read_file");
        assert!(tc.args_json.contains("\"path\""));
    }

    #[test]
    fn parse_output_thinking() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anthropic_output_thinking.json"
        ))
        .unwrap();
        let out = AnthropicWireApi.parse_output(&body);
        assert!(out
            .reasoning
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        assert!(out
            .message
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn parse_input_user_only() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anthropic_input_user_only.json"
        ))
        .unwrap();
        let out = AnthropicWireApi.parse_input(&body);
        assert!(out.user_message.is_some());
        assert!(out.tool_results.is_empty());
    }

    #[test]
    fn parse_input_with_tool_result() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anthropic_input_with_tool_result.json"
        ))
        .unwrap();
        let out = AnthropicWireApi.parse_input(&body);
        assert_eq!(out.tool_results.len(), 1);
        assert!(out.tool_results[0].tool_use_id.starts_with("toolu_"));
        assert!(!out.tool_results[0].content.is_empty());
    }

    fn anthropic_full_input() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/anthropic_input_full.json"
        ))
        .unwrap()
    }

    #[test]
    fn parse_input_full_system_extracted() {
        let out = AnthropicWireApi.parse_input(&anthropic_full_input());
        assert_eq!(out.system.as_deref(), Some("You are a helpful assistant."));
    }

    #[test]
    fn parse_input_full_messages_order_and_roles() {
        use crate::model::{ParsedContentBlock, ParsedRole};
        let out = AnthropicWireApi.parse_input(&anthropic_full_input());
        // Message 3 ("role":"user", content is exclusively tool_result) should be re-tagged to Tool.
        let roles: Vec<ParsedRole> = out.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                ParsedRole::User,      // "hello"
                ParsedRole::Assistant, // text + tool_use
                ParsedRole::Tool,      // tool_result-only
                ParsedRole::User,      // mixed image + text
            ]
        );
        // The assistant message has exactly 2 content blocks: Text + ToolUse.
        let assistant = &out.messages[1];
        assert_eq!(assistant.content.len(), 2);
        assert!(matches!(
            assistant.content[0],
            ParsedContentBlock::Text { .. }
        ));
        assert!(matches!(
            &assistant.content[1],
            ParsedContentBlock::ToolUse { name, .. } if name == "read_file"
        ));
        // The tool-role message has one ToolResult block.
        let tool_msg = &out.messages[2];
        assert_eq!(tool_msg.content.len(), 1);
        assert!(matches!(
            &tool_msg.content[0],
            ParsedContentBlock::ToolResult { tool_use_id, content, is_error }
                if tool_use_id == "toolu_abc" && content == "file bytes" && !*is_error
        ));
        // The last user message has Image + Text.
        let last = &out.messages[3];
        assert_eq!(last.content.len(), 2);
        assert!(matches!(
            &last.content[0],
            ParsedContentBlock::Image { mime, .. } if mime.as_deref() == Some("image/png")
        ));
        assert!(matches!(&last.content[1], ParsedContentBlock::Text { .. }));
    }

    #[test]
    fn parse_input_full_tools_extracted() {
        let out = AnthropicWireApi.parse_input(&anthropic_full_input());
        assert_eq!(out.tools.len(), 2);
        assert_eq!(out.tools[0].name, "read_file");
        assert_eq!(
            out.tools[0].description.as_deref(),
            Some("Read the contents of a file.")
        );
        // input_schema stored as a JSON string — parse it back to verify shape.
        let schema: serde_json::Value =
            serde_json::from_str(&out.tools[0].input_schema_json).unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn parse_input_full_sampling_extracted() {
        let out = AnthropicWireApi.parse_input(&anthropic_full_input());
        assert_eq!(out.sampling.temperature, Some(0.7));
        assert_eq!(out.sampling.top_p, Some(0.95));
        assert_eq!(out.sampling.max_tokens, Some(8192));
        assert_eq!(out.sampling.stream, Some(true));
        assert_eq!(out.sampling.stop, vec!["STOP".to_string()]);
        // tool_choice was an object; stored as serialized JSON string.
        assert_eq!(
            out.sampling.tool_choice.as_deref(),
            Some(r#"{"type":"auto"}"#)
        );
    }

    #[test]
    fn parse_input_preserves_unknown_content_block() {
        use crate::model::ParsedContentBlock;
        let body = serde_json::json!({
            "model": "claude-3",
            "system": "s",
            "messages": [
                { "role": "user", "content": [ { "type": "future_kind", "foo": 1 } ] }
            ]
        });
        let out = AnthropicWireApi.parse_input(&body);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].content.len(), 1);
        assert!(matches!(
            out.messages[0].content[0],
            ParsedContentBlock::Unknown(_)
        ));
    }
}
