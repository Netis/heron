use serde_json::Value;

use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{FinishReason, Provider, RequestInfo, ResponseInfo};

/// Provider implementation for OpenAI Chat Completions API.
pub struct OpenAiChatProvider;

impl Provider for OpenAiChatProvider {
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

/// Provider implementation for OpenAI Responses API.
/// Shares extraction logic with Chat Completions.
pub struct OpenAiResponsesProvider;

impl Provider for OpenAiResponsesProvider {
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

/// Extract request info from an OpenAI API request (both Chat and Responses).
pub fn extract_from_request(req: &HttpRequestData) -> RequestInfo {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Chat Completions defaults to non-streaming; explicit "stream": true opts in.
    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Tenant ID: prefix of the API key (first 10 chars) for tenant differentiation.
    // Not a cryptographic hash — sufficient for grouping, not for security.
    let tenant_id = req.header("authorization").map(|auth| {
        let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
        let prefix = if token.len() > 10 {
            &token[..10]
        } else {
            token
        };
        prefix.to_string()
    });

    RequestInfo {
        model,
        is_stream,
        tenant_id,
    }
}

/// Extract response info from a non-streaming OpenAI Chat Completions response.
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

    // Chat Completions: choices[0].finish_reason
    let finish_reason = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(map_chat_finish_reason);

    let input_tokens = body
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let output_tokens = body
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let total_tokens = body
        .get("usage")
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let cache_read_input_tokens = body
        .get("usage")
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

/// Extract response info from SSE events (OpenAI Responses API).
pub fn extract_from_sse(sse_events: &[SseEventData]) -> ResponseInfo {
    let mut model: Option<String> = None;
    let mut finish_reason: Option<FinishReason> = None;
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut total_tokens: Option<u32> = None;
    let mut cache_read_input_tokens: Option<u32> = None;
    let mut response_body: Option<String> = None;
    let mut response_id: Option<String> = None;
    // Codex (Responses API) streams output items via response.output_item.done
    // events; the terminal response.completed event carries an empty `output`
    // array. We accumulate finalized items here and graft them into the
    // response_body so downstream consumers (turn-terminal predicate, assistant
    // text extraction) see the full output.
    let mut output_items: Vec<Value> = Vec::new();

    for event in sse_events {
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);

        match event.event_type.as_str() {
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                // The response.completed event contains the full response object.
                if let Some(response) = data.get("response") {
                    if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                        response_id = Some(id.to_string());
                    }
                    if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                    if let Some(status) = response.get("status").and_then(|v| v.as_str()) {
                        finish_reason = Some(map_responses_status(status));
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
                        if let Some(cr) = usage.get("prompt_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_input_tokens = Some(cr as u32);
                        }
                    }
                    // Graft accumulated output items into the response object
                    // when the wire copy is empty (codex behavior).
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
            "response.created" => {
                // response.created also contains the response object with model.
                if let Some(response) = data.get("response") {
                    if model.is_none() {
                        if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                            model = Some(m.to_string());
                        }
                    }
                    if response_id.is_none() {
                        if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                            response_id = Some(id.to_string());
                        }
                    }
                }
            }
            // Chat Completions: chunks with no event_type may contain usage
            // in the final chunk (when stream_options: {"include_usage": true}).
            _ => {
                let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);

                // Chat Completions chunks: extract id from first chunk
                if response_id.is_none() {
                    if let Some(id) = data.get("id").and_then(|v| v.as_str()) {
                        response_id = Some(id.to_string());
                    }
                }

                // Chat Completions chunks: extract model
                if model.is_none() {
                    if let Some(m) = data.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                }

                // Chat Completions chunks: extract finish_reason from choices[0]
                if finish_reason.is_none() {
                    if let Some(fr) = data
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(|v| v.as_str())
                    {
                        finish_reason = Some(map_chat_finish_reason(fr));
                    }
                }

                // Chat Completions final chunk may include usage
                if input_tokens.is_none() {
                    if let Some(usage) = data.get("usage").filter(|u| u.is_object()) {
                        if let Some(it) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                            input_tokens = Some(it as u32);
                        }
                        if let Some(ot) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                            output_tokens = Some(ot as u32);
                        }
                        if let Some(tt) = usage.get("total_tokens").and_then(|v| v.as_u64()) {
                            total_tokens = Some(tt as u32);
                        }
                        if let Some(cr) = usage.get("prompt_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_input_tokens = Some(cr as u32);
                        }
                    }
                }
            }
        }
    }

    // If no response.completed event was found (e.g. Chat Completions streaming),
    // try to assemble from chat-style SSE chunks.
    if response_body.is_none() {
        response_body = build_chat_response_body(sse_events);
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

/// Assemble a synthetic response body from OpenAI Chat Completions streaming chunks.
///
/// Chat Completions streaming sends chunks with `choices[0].delta.content` for text
/// and `choices[0].delta.tool_calls` for tool use. The last chunk has
/// `choices[0].finish_reason`. No event types — just `data: {...}` lines.
fn build_chat_response_body(sse_events: &[SseEventData]) -> Option<String> {
    use serde_json::json;

    let mut model: Option<String> = None;
    let mut content = String::new();
    let mut finish_reason: Option<String> = None;
    let mut tool_calls: std::collections::BTreeMap<u64, (String, String, String)> =
        std::collections::BTreeMap::new();
    let mut has_data = false;

    for event in sse_events {
        if event.data.trim() == "[DONE]" {
            continue;
        }
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);
        if !data.is_object() {
            continue;
        }
        has_data = true;

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
                // Accumulate tool calls by index
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

        // Also handle usage in the final chunk (stream_options: include_usage)
        // Skipped here since usage is already extracted in the main function.
    }

    if !has_data {
        return None;
    }

    // Build the synthetic message
    let mut message = json!({ "role": "assistant" });
    if !content.is_empty() {
        message["content"] = Value::String(content);
    }
    if !tool_calls.is_empty() {
        let tc_array: Vec<Value> = tool_calls
            .into_iter()
            .map(|(_, (id, name, args))| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args }
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(tc_array);
    }

    let result = json!({
        "model": model.unwrap_or_default(),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }]
    });

    Some(result.to_string())
}

/// Map OpenAI Chat Completions finish_reason to normalized FinishReason.
fn map_chat_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Complete,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::Cancelled,
        _ => FinishReason::Error,
    }
}

/// Map OpenAI Responses API status to normalized FinishReason.
fn map_responses_status(status: &str) -> FinishReason {
    match status {
        "completed" => FinishReason::Complete,
        "failed" => FinishReason::Error,
        "cancelled" => FinishReason::Cancelled,
        "incomplete" => FinishReason::Length,
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
    fn test_map_chat_finish_reason() {
        assert_eq!(map_chat_finish_reason("stop"), FinishReason::Complete);
        assert_eq!(map_chat_finish_reason("tool_calls"), FinishReason::ToolUse);
        assert_eq!(map_chat_finish_reason("length"), FinishReason::Length);
    }

    #[test]
    fn test_map_responses_status() {
        assert_eq!(map_responses_status("completed"), FinishReason::Complete);
        assert_eq!(map_responses_status("failed"), FinishReason::Error);
        assert_eq!(map_responses_status("incomplete"), FinishReason::Length);
    }

    #[test]
    fn test_build_chat_response_body_text() {
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
        let body = build_chat_response_body(&events).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "gpt-4");
        assert_eq!(v["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_build_chat_response_body_tool_calls() {
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
        let body = build_chat_response_body(&events).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        let tc = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["function"]["name"], "get_weather");
        // arguments must be a string, not a parsed object
        assert!(tc["function"]["arguments"].is_string());
        assert_eq!(tc["function"]["arguments"], r#"{"city":"SF"}"#);
    }

    #[test]
    fn test_build_chat_response_body_empty() {
        let events: Vec<SseEventData> = vec![];
        assert!(build_chat_response_body(&events).is_none());
    }

    #[test]
    fn test_extract_request_stream_default_false() {
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let body = serde_json::json!({"model": "gpt-4"});
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
        let info = extract_from_request(&req);
        // When "stream" is absent, Chat Completions defaults to false
        assert!(
            !info.is_stream,
            "stream should default to false for Chat Completions"
        );
    }

    #[test]
    fn test_extract_from_sse_chat_completions_finish_reason() {
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
        let info = extract_from_sse(&events);
        assert_eq!(
            info.finish_reason,
            Some(FinishReason::Complete),
            "finish_reason must be extracted from Chat Completions streaming chunks"
        );
    }

    #[test]
    fn test_extract_from_sse_chat_completions_finish_reason_tool_calls() {
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
        let info = extract_from_sse(&events);
        assert_eq!(info.finish_reason, Some(FinishReason::ToolUse));
    }

    #[test]
    fn test_extract_from_sse_chat_completions_usage() {
        // Chat Completions final chunk with usage (stream_options: include_usage)
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
        let info = extract_from_sse(&events);
        assert_eq!(info.input_tokens, Some(10));
        assert_eq!(info.output_tokens, Some(5));
        assert_eq!(info.total_tokens, Some(15));
    }

    #[test]
    fn test_extract_response_id_non_streaming() {
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let body = serde_json::json!({
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
        let info = extract_from_response(&resp);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-abc123"));
    }

    #[test]
    fn test_extract_response_id_chat_completions_streaming() {
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
        let info = extract_from_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-stream1"));
    }

    #[test]
    fn test_responses_api_grafts_output_items_into_empty_response_output() {
        // Real-codex behavior: response.completed.response.output is empty;
        // items arrive via response.output_item.done. Parser must accumulate
        // them and graft into the response_body so downstream consumers
        // (turn-terminal predicate, assistant-text extraction) can inspect them.
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
        let info = extract_from_sse(&events);
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
    fn test_responses_api_preserves_nonempty_response_output() {
        // If response.completed already carries output (older/alternative codex
        // behavior), we must NOT overwrite it with the accumulated items.
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
        let info = extract_from_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let out = v.get("output").and_then(|o| o.as_array()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("type").and_then(|t| t.as_str()), Some("message"));
    }

    #[test]
    fn test_extract_response_id_responses_api_streaming() {
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
        let info = extract_from_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("resp_xyz"));
    }
}
