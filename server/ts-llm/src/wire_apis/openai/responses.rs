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

use crate::model::{FinishReason, RequestInfo, ResponseInfo, RouteVerdict, WireApi};

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
        .map(map_finish_reason);

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
    let mut finish_reason: Option<FinishReason> = None;
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
                        finish_reason = Some(map_finish_reason(status));
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
                            map.insert(
                                "output".to_string(),
                                Value::Array(output_items.clone()),
                            );
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

fn map_finish_reason(status: &str) -> FinishReason {
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
    fn test_map_finish_reason() {
        assert_eq!(map_finish_reason("completed"), FinishReason::Complete);
        assert_eq!(map_finish_reason("failed"), FinishReason::Error);
        assert_eq!(map_finish_reason("incomplete"), FinishReason::Length);
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
        assert_eq!(info.finish_reason, Some(FinishReason::Complete));
        assert_eq!(info.input_tokens, Some(10));
        assert_eq!(info.output_tokens, Some(5));
        assert_eq!(info.total_tokens, Some(15));
        assert_eq!(info.cache_read_input_tokens, Some(3));
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
