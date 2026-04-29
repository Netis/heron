//! Generic OpenAI Chat Completions profile — matches /v1/chat/completions
//! traffic from any client. Synthesizes session_id from messages history
//! (call #2+) or response_body (call #1).

use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_apis as wa;
use serde_json::Value;

use super::generic_common::{compose_session_id_tracked, AssistantSig};

pub struct GenericOpenAiChatProfile;

fn user_content_to_text(content: &Value) -> Option<String> {
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
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        _ => None,
    }
}

fn first_user_text(msgs: &[Value]) -> Option<String> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        if let Some(t) = m.get("content").and_then(user_content_to_text) {
            return Some(t);
        }
    }
    None
}

fn first_assistant_sig_from_request(msgs: &[Value]) -> Option<AssistantSig> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(arr) = m.get("tool_calls").and_then(|v| v.as_array()) {
            if let Some(id) = arr.first().and_then(|tc| tc.get("id")).and_then(|v| v.as_str()) {
                return Some(AssistantSig::ToolId(id.to_string()));
            }
        }
        if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
            if !c.trim().is_empty() {
                return Some(AssistantSig::Text(c.to_string()));
            }
        }
    }
    None
}

fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let v: Value = serde_json::from_str(body).ok()?;
    let msg = v.get("choices")?.get(0)?.get("message")?;
    if let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        if let Some(id) = arr.first().and_then(|tc| tc.get("id")).and_then(|v| v.as_str()) {
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

impl AgentProfile for GenericOpenAiChatProfile {
    fn name(&self) -> &'static str {
        "generic-openai-chat"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        call.wire_api == wa::OPENAI_CHAT
    }

    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let msgs = v.get("messages")?.as_array()?;
        let user_text = first_user_text(msgs)?;
        let sig = first_assistant_sig_from_request(msgs)
            .or_else(|| call.response_body.as_deref().and_then(first_assistant_sig_from_response))?;
        let (session_id, tool_id_canonicalized) = compose_session_id_tracked(&user_text, sig);
        Some(ExtractedIds { session_id, tool_id_canonicalized })
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        // Last message is role=user with non-empty content (user content blocks
        // in Chat Completions don't have a "tool_result" type — tool results
        // are role=tool messages, which are not user-turn-start by definition).
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        Some(last.get("content").and_then(user_content_to_text).is_some())
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return None;
        }
        last.get("content").and_then(user_content_to_text)
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let c = v.get("choices")?.get(0)?.get("message")?.get("content")?.as_str()?;
        if c.trim().is_empty() {
            None
        } else {
            Some(c.to_string())
        }
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
            wire_api: wa::OPENAI_CHAT,
            model: "gpt".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/chat/completions".into(),
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
    fn matches_chat_only() {
        let mut c = call_with(None, None);
        assert!(GenericOpenAiChatProfile.matches(&c));
        c.wire_api = wa::ANTHROPIC;
        assert!(!GenericOpenAiChatProfile.matches(&c));
    }

    #[test]
    fn extract_ids_call_n_with_tool_history() {
        let req = r#"{"messages":[
            {"role":"system","content":"you are helpful"},
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_abc","content":"ok"}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiChatProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "call_abc");
    }

    #[test]
    fn extract_ids_call_1_tool_in_response_canonicalized() {
        // Simulate OpenClaw: tool_id without underscore in echo, but response
        // (call #1 fallback path) gives canonical form. Both must produce same id.
        let req1 = r#"{"messages":[{"role":"user","content":"x"}]}"#;
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#;
        let req2 = r#"{"messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"callabc","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"callabc","content":"ok"}
        ]}"#;
        let c1 = call_with(Some(req1), Some(resp));
        let c2 = call_with(Some(req2), None);
        let id1 = GenericOpenAiChatProfile.extract_ids(&c1).unwrap().session_id;
        let id2 = GenericOpenAiChatProfile.extract_ids(&c2).unwrap().session_id;
        assert_eq!(id1, "call_abc");
        assert_eq!(id1, id2, "call #1 (canonical) and call #2 (stripped) must canonicalize to same id");
    }

    #[test]
    fn extract_ids_call_n_with_text_only_history() {
        let req = r#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"hello"},
            {"role":"user","content":"more"}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiChatProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_call_1_text_in_response() {
        let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let c = call_with(Some(req), Some(resp));
        assert!(GenericOpenAiChatProfile.extract_ids(&c).unwrap().session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_none_when_first_call_no_response() {
        let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let c = call_with(Some(req), None);
        assert!(GenericOpenAiChatProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn extract_ids_none_when_malformed_json() {
        let c = call_with(Some("garbage"), None);
        assert!(GenericOpenAiChatProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn first_user_skips_system() {
        // OpenAI Chat commonly puts system at index 0 — first user text walker
        // must skip it.
        let req = r#"{"messages":[
            {"role":"system","content":"you are X"},
            {"role":"user","content":"actual prompt"}
        ]}"#;
        let v: Value = serde_json::from_str(req).unwrap();
        let msgs = v.get("messages").unwrap().as_array().unwrap();
        assert_eq!(first_user_text(msgs).as_deref(), Some("actual prompt"));
    }

    #[test]
    fn is_user_turn_start_text() {
        let req = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
        assert_eq!(GenericOpenAiChatProfile.is_user_turn_start(&call_with(Some(req), None)), Some(true));
    }

    #[test]
    fn is_user_turn_start_false_when_last_is_tool() {
        let req = r#"{"messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_a","content":"ok"}
        ]}"#;
        assert_eq!(GenericOpenAiChatProfile.is_user_turn_start(&call_with(Some(req), None)), Some(false));
    }
}
