//! OpenAI Chat Completions wire API (`POST /v1/chat/completions`).
//!
//! Strict to the Chat shape — `choices[0].finish_reason`,
//! `usage.prompt_tokens`, `usage.completion_tokens`,
//! `usage.prompt_tokens_details.cached_tokens`. No `.or_else()` fallback to
//! the Responses shape: this module only runs once the registry has already
//! selected us, so ambiguity would be a bug, not something to tolerate.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{FinishReason, RequestInfo, ResponseInfo, RouteVerdict, WireApi};

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

    fn matches_shape(&self, _req: &HttpRequestData, body: &Value) -> bool {
        // Chat Completions: `model` + non-empty `messages[]`. Presence of
        // `input` means the Responses API, not us.
        body.get("model").and_then(|v| v.as_str()).is_some()
            && body.get("input").is_none()
            && body
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
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
}

fn extract_request(_req: &HttpRequestData, body: &Value) -> RequestInfo {
    RequestInfo {
        model: body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        // Chat Completions defaults to non-streaming; explicit opt-in via "stream": true.
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
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(map_finish_reason);

    let usage = body.get("usage");
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
fn extract_sse(events: &[SseEventData]) -> ResponseInfo {
    let mut response_id: Option<String> = None;
    let mut model: Option<String> = None;
    let mut finish_reason: Option<FinishReason> = None;
    // Preserve the raw `finish_reason` string for the synthetic body so
    // downstream consumers see what the wire said, not our normalized enum.
    let mut finish_reason_str: Option<String> = None;
    let mut content = String::new();
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
                finish_reason = Some(map_finish_reason(fr));
                finish_reason_str = Some(fr.to_string());
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                    content.push_str(c);
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

    let response_body = if saw_chunk {
        Some(build_synthetic_body(
            model.as_deref(),
            &content,
            tool_calls,
            finish_reason_str.as_deref(),
            input_tokens,
            output_tokens,
            total_tokens,
            cache_read_input_tokens,
        ))
    } else {
        None
    };

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

/// Compose a non-streaming-shaped response body from accumulated deltas so
/// downstream consumers don't need a separate streaming reader.
#[allow(clippy::too_many_arguments)]
fn build_synthetic_body(
    model: Option<&str>,
    content: &str,
    tool_calls: BTreeMap<u64, (String, String, String)>,
    finish_reason: Option<&str>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    total_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
) -> String {
    let mut message = json!({ "role": "assistant" });
    if !content.is_empty() {
        message["content"] = Value::String(content.to_string());
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

    result.to_string()
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Complete,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::Cancelled,
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
    fn test_map_finish_reason() {
        assert_eq!(map_finish_reason("stop"), FinishReason::Complete);
        assert_eq!(map_finish_reason("tool_calls"), FinishReason::ToolUse);
        assert_eq!(map_finish_reason("length"), FinishReason::Length);
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
        let info = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "gpt-4");
        assert_eq!(v["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
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
        let info = extract_sse(&events);
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
        let info = extract_sse(&[]);
        assert!(info.response_body.is_none());
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
        let info = extract_sse(&events);
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
        let body_v = serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
        let info = extract_request(&req, &body_v);
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
        let info = extract_sse(&events);
        assert_eq!(info.finish_reason, Some(FinishReason::Complete));
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
        let info = extract_sse(&events);
        assert_eq!(info.finish_reason, Some(FinishReason::ToolUse));
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
        let info = extract_sse(&events);
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
        let info = extract_response(&resp);
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
        let info = extract_sse(&events);
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
        let info = extract_sse(&events);
        assert_eq!(info.cache_read_input_tokens, Some(7));
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
        let info = extract_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(info.model.as_deref(), Some("gpt-4"));
        assert_eq!(info.finish_reason, Some(FinishReason::Complete));
    }
}
