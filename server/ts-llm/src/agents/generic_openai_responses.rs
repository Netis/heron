//! Generic OpenAI Responses profile — matches /v1/responses traffic that
//! does NOT carry codex-tui's UA or X-Codex-Turn-Metadata header.
//!
//! Uses the same `body_has_terminal_message_only` helper as CodexCliProfile
//! for `is_turn_terminal`, since the wire-api `status: "completed"` is
//! unreliable for this protocol regardless of which client emitted the call.

use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;
use serde_json::Value;

use super::generic_common::{compose_session_id_tracked, AssistantSig};

pub struct GenericOpenAiResponsesProfile;

fn message_text(item: &Value) -> Option<String> {
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
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        _ => None,
    }
}

fn first_user_text(items: &[Value]) -> Option<String> {
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

fn first_assistant_sig_from_input(items: &[Value]) -> Option<AssistantSig> {
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

fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let v: Value = serde_json::from_str(body).ok()?;
    let output = v.get("output")?.as_array()?;
    first_assistant_sig_from_input(output)
}

impl AgentProfile for GenericOpenAiResponsesProfile {
    fn name(&self) -> &'static str {
        "generic-openai-responses"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        call.wire_api == wa::OPENAI_RESPONSES
    }

    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        // input may be array (full mode) or string (simplified mode).
        let (user_text, sig_from_input) = match v.get("input")? {
            Value::Array(items) => (first_user_text(items), first_assistant_sig_from_input(items)),
            Value::String(s) if !s.trim().is_empty() => (Some(s.clone()), None),
            _ => (None, None),
        };
        let user_text = user_text?;
        let sig = sig_from_input
            .or_else(|| call.response_body.as_deref().and_then(first_assistant_sig_from_response))?;
        let (session_id, tool_id_canonicalized) = compose_session_id_tracked(&user_text, sig);
        Some(ExtractedIds { session_id, tool_id_canonicalized })
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        // Last item in input is type=message, role=user with non-empty text?
        // function_call_output items break user-turn-start.
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let items = match v.get("input")? {
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

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        match v.get("input")? {
            Value::Array(items) => {
                // Reverse-walk for the latest type=message, role=user.
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

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let output = v.get("output")?.as_array()?;
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

    fn is_turn_terminal(&self, call: &LlmCall, _wire_apis: &WireApiRegistry) -> bool {
        // Same protocol-level reasoning as CodexCliProfile.
        crate::wire_apis::openai::body_has_terminal_message_only(
            call.response_body.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(req: Option<&str>, resp: Option<&str>) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api: wa::OPENAI_RESPONSES,
            model: "gpt".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/responses".into(),
            is_stream: true,
            request_body: req.map(str::to_string),
            status_code: None,
            finish_reason: None,
            response_body: resp.map(str::to_string),
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
        }
    }

    #[test]
    fn matches_responses_only() {
        let mut c = call_with(None, None);
        assert!(GenericOpenAiResponsesProfile.matches(&c));
        c.wire_api = wa::OPENAI_CHAT;
        assert!(!GenericOpenAiResponsesProfile.matches(&c));
    }

    #[test]
    fn extract_ids_call_n_with_function_call() {
        // Codex-shape input: developer + user + reasoning + assistant + function_call.
        let req = r#"{"input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"sys"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"reasoning","summary":[],"content":[]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_xyz"}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiResponsesProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "fc_xyz", "function_call.call_id wins over assistant text");
    }

    #[test]
    fn extract_ids_call_1_function_call_in_response() {
        let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let resp = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_abc"}]}"#;
        let c = call_with(Some(req), Some(resp));
        let ids = GenericOpenAiResponsesProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "fc_abc");
    }

    #[test]
    fn extract_ids_call_1_and_n_match() {
        let req1 = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]}]}"#;
        let resp = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"}]}"#;
        let req2 = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"},
            {"type":"function_call_output","call_id":"fc_same","output":"ok"}
        ]}"#;
        let c1 = call_with(Some(req1), Some(resp));
        let c2 = call_with(Some(req2), None);
        assert_eq!(
            GenericOpenAiResponsesProfile.extract_ids(&c1).unwrap().session_id,
            GenericOpenAiResponsesProfile.extract_ids(&c2).unwrap().session_id,
        );
    }

    #[test]
    fn extract_ids_call_n_with_text_only_assistant() {
        let req = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiResponsesProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_input_string_mode_treats_as_call_1() {
        // Simplified mode: input is a string. No assistant in input → falls through to response.
        let req = r#"{"input":"just a prompt"}"#;
        let resp = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_simple"}]}"#;
        let c = call_with(Some(req), Some(resp));
        assert_eq!(GenericOpenAiResponsesProfile.extract_ids(&c).unwrap().session_id, "fc_simple");
    }

    #[test]
    fn extract_ids_none_when_string_input_and_no_response() {
        // Symmetric to extract_ids_none_when_first_call_no_response but for the
        // string-mode input path: no assistant in input (string mode never has
        // any), no response_body to fall back to → None.
        let req = r#"{"input":"just a prompt"}"#;
        let c = call_with(Some(req), None);
        assert!(GenericOpenAiResponsesProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn extract_ids_none_when_first_call_no_response() {
        let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let c = call_with(Some(req), None);
        assert!(GenericOpenAiResponsesProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn extract_ids_none_when_malformed_json() {
        let c = call_with(Some("garbage"), None);
        assert!(GenericOpenAiResponsesProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn is_turn_terminal_delegates_to_helper() {
        // message-only output → terminal.
        let resp = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}"#;
        let mut c = call_with(None, Some(resp));
        let wires = crate::wire_apis::build_default_wire_api_registry();
        assert!(GenericOpenAiResponsesProfile.is_turn_terminal(&c, &wires));
        // function_call → not terminal.
        c.response_body = Some(r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#.to_string());
        assert!(!GenericOpenAiResponsesProfile.is_turn_terminal(&c, &wires));
    }

    #[test]
    fn is_user_turn_start_last_user_message() {
        let req = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
            {"type":"function_call_output","call_id":"fc_a","output":"ok"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"more"}]}
        ]}"#;
        assert_eq!(GenericOpenAiResponsesProfile.is_user_turn_start(&call_with(Some(req), None)), Some(true));
    }

    #[test]
    fn is_user_turn_start_false_when_last_is_function_call_output() {
        let req = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
            {"type":"function_call_output","call_id":"fc_a","output":"ok"}
        ]}"#;
        assert_eq!(GenericOpenAiResponsesProfile.is_user_turn_start(&call_with(Some(req), None)), Some(false));
    }
}
