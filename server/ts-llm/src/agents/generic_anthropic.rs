//! Generic Anthropic Messages profile — matches /v1/messages traffic that
//! does NOT carry claude-cli's user-agent. Synthesizes session_id from
//! the messages history (call #2+) or response body (call #1). No
//! sub-agent or auxiliary classification — those signals are
//! claude-cli-specific.

use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_apis as wa;
use serde_json::Value;

use super::generic_common::{compose_session_id_tracked, AssistantSig};

pub struct GenericAnthropicProfile;

fn first_user_text(msgs: &[Value]) -> Option<String> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        match m.get("content")? {
            Value::String(s) if !s.trim().is_empty() => return Some(s.clone()),
            Value::Array(blocks) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
                    .collect();
                if !parts.is_empty() {
                    return Some(parts.join("\n"));
                }
            }
            _ => {}
        }
    }
    None
}

fn first_assistant_sig_from_request(msgs: &[Value]) -> Option<AssistantSig> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let blocks = m.get("content")?.as_array()?;
        // Prefer first tool_use id.
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                    return Some(AssistantSig::ToolId(id.to_string()));
                }
            }
        }
        // Fall back to joined text.
        let parts: Vec<String> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect();
        if !parts.is_empty() {
            return Some(AssistantSig::Text(parts.join("\n")));
        }
    }
    None
}

fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let v: Value = serde_json::from_str(body).ok()?;
    let blocks = v.get("content")?.as_array()?;
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
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

impl AgentProfile for GenericAnthropicProfile {
    fn name(&self) -> &'static str {
        "generic-anthropic"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        call.wire_api == wa::ANTHROPIC
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
        // Last user message contains at least one non-tool_result block?
        // No <system-reminder> stripping (that's claude-cli-specific).
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        match last.get("content")? {
            Value::String(s) => Some(!s.trim().is_empty()),
            Value::Array(blocks) => Some(blocks.iter().any(|b| {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => false,
                    Some("text") => b.get("text").and_then(|t| t.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false),
                    Some(_) => true, // image, future block types — count as user-visible
                    None => false,
                }
            })),
            _ => None,
        }
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        // Last user message text (no system-reminder stripping).
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
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

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let blocks = v.get("content")?.as_array()?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(headers: Vec<(&str, &str)>, req: Option<&str>, resp: Option<&str>) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api: wa::ANTHROPIC,
            model: "claude".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/messages".into(),
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
            request_headers: headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            response_headers: vec![],
        }
    }

    #[test]
    fn matches_anthropic_no_ua_required() {
        let c = call_with(vec![], None, None);
        assert!(GenericAnthropicProfile.matches(&c));
    }

    #[test]
    fn does_not_match_other_wire_api() {
        let mut c = call_with(vec![], None, None);
        c.wire_api = wa::OPENAI_CHAT;
        assert!(!GenericAnthropicProfile.matches(&c));
    }

    #[test]
    fn extract_ids_call_n_with_tool_history() {
        let req = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"ok"}]}
        ]}"#;
        let c = call_with(vec![], Some(req), None);
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "toolu_abc");
    }

    #[test]
    fn extract_ids_call_n_with_text_only_history() {
        let req = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"text","text":"hello there"}]},
            {"role":"user","content":[{"type":"text","text":"more"}]}
        ]}"#;
        let c = call_with(vec![], Some(req), None);
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
        assert_eq!(ids.session_id.len(), "gen-".len() + 16);
    }

    #[test]
    fn extract_ids_call_1_tool_in_response() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let resp = r#"{"content":[{"type":"tool_use","id":"toolu_xyz","name":"Read","input":{}}]}"#;
        let c = call_with(vec![], Some(req), Some(resp));
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "toolu_xyz");
    }

    #[test]
    fn extract_ids_call_1_text_in_response() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let resp = r#"{"content":[{"type":"text","text":"hello there"}]}"#;
        let c = call_with(vec![], Some(req), Some(resp));
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_call_1_and_n_match() {
        // Call #1 sees response_body; call #2 sees the same content echoed in messages[1].
        let resp = r#"{"content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]}"#;
        let req1 = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"prompt"}]}]}"#;
        let req2 = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"prompt"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_same","content":"ok"}]}
        ]}"#;
        let c1 = call_with(vec![], Some(req1), Some(resp));
        let c2 = call_with(vec![], Some(req2), None);
        let id1 = GenericAnthropicProfile.extract_ids(&c1).unwrap().session_id;
        let id2 = GenericAnthropicProfile.extract_ids(&c2).unwrap().session_id;
        assert_eq!(id1, id2, "call #1 and call #2 must synthesize same session_id");
    }

    #[test]
    fn extract_ids_call_1_with_normalized_tool_id() {
        // Response stream emits canonical form; some hypothetical client strips the underscore in echo.
        let resp = r#"{"content":[{"type":"tool_use","id":"toolu_abc","name":"R","input":{}}]}"#;
        let req2 = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"x"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"tooluabc","name":"R","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"tooluabc","content":"ok"}]}
        ]}"#;
        let c1 = call_with(vec![], Some(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#), Some(resp));
        let c2 = call_with(vec![], Some(req2), None);
        assert_eq!(
            GenericAnthropicProfile.extract_ids(&c1).unwrap().session_id,
            GenericAnthropicProfile.extract_ids(&c2).unwrap().session_id,
        );
    }

    #[test]
    fn extract_ids_none_when_first_call_no_response() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let c = call_with(vec![], Some(req), None);
        assert!(GenericAnthropicProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn extract_ids_none_when_malformed_json() {
        let c = call_with(vec![], Some("garbage"), None);
        assert!(GenericAnthropicProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn is_user_turn_start_text() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
        let c = call_with(vec![], Some(req), None);
        assert_eq!(GenericAnthropicProfile.is_user_turn_start(&c), Some(true));
    }

    #[test]
    fn is_user_turn_start_tool_result_only() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
        let c = call_with(vec![], Some(req), None);
        assert_eq!(GenericAnthropicProfile.is_user_turn_start(&c), Some(false));
    }
}
