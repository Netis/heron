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

    RequestInfo { model, is_stream }
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
}
