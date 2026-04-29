//! Generic agent profile — synthesizes a session id from the request /
//! response payload alone, so TokenScope produces `AgentTurn`s for header-less
//! LLM traffic across all three supported wire APIs (Anthropic Messages,
//! OpenAI Chat Completions, OpenAI Responses).
//!
//! `agent_kind == "generic"` is wire-api-agnostic; downstream consumers read
//! `wire_api` separately. Per-shape parsing lives in the three private
//! sub-modules (`anthropic`, `openai_chat`, `openai_responses`); the public
//! profile dispatches on `call.wire_api`. Mirrors the `openclaw.rs` pattern.

use crate::model::LlmCall;
use crate::profile::{AgentProfile, SessionIdExtraction};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;

use super::generic_common::{compose_session_id_tracked, AssistantSig};

pub struct GenericProfile;

// ───────────────────────── Anthropic Messages shape ─────────────────────────
mod anthropic {
    use super::AssistantSig;
    use serde_json::Value;

    pub fn first_user_text(msgs: &[Value]) -> Option<String> {
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

    pub fn first_assistant_sig_from_request(msgs: &[Value]) -> Option<AssistantSig> {
        for m in msgs {
            if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                continue;
            }
            let blocks = m.get("content")?.as_array()?;
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
            if !parts.is_empty() {
                return Some(AssistantSig::Text(parts.join("\n")));
            }
        }
        None
    }

    pub fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
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
}

// ────────────────────── OpenAI Chat Completions shape ──────────────────────
mod openai_chat {
    use super::AssistantSig;
    use serde_json::Value;

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

    pub fn first_user_text(msgs: &[Value]) -> Option<String> {
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

    pub fn first_assistant_sig_from_request(msgs: &[Value]) -> Option<AssistantSig> {
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

    pub fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
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
}

// ──────────────────────── OpenAI Responses shape ────────────────────────────
mod openai_responses {
    use super::AssistantSig;
    use serde_json::Value;

    pub fn message_text(item: &Value) -> Option<String> {
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
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join("\n"))
                }
            }
            _ => None,
        }
    }

    pub fn first_user_text(items: &[Value]) -> Option<String> {
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

    pub fn first_assistant_sig_from_input(items: &[Value]) -> Option<AssistantSig> {
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

    pub fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
        let v: Value = serde_json::from_str(body).ok()?;
        let output = v.get("output")?.as_array()?;
        first_assistant_sig_from_input(output)
    }
}

// ──────────────────────── Per-wire-api method bodies ────────────────────────
//
// Kept as free functions on `GenericProfile` (rather than inlined into the
// trait impl) so the dispatch in each method stays one match arm wide.

impl GenericProfile {
    fn extract_session_id_anthropic(call: &LlmCall) -> Option<SessionIdExtraction> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let msgs = v.get("messages")?.as_array()?;
        let user_text = anthropic::first_user_text(msgs)?;
        let sig = anthropic::first_assistant_sig_from_request(msgs).or_else(|| {
            call.response_body
                .as_deref()
                .and_then(anthropic::first_assistant_sig_from_response)
        })?;
        let (session_id, tool_id_canonicalized) = compose_session_id_tracked(&user_text, sig);
        Some(SessionIdExtraction {
            session_id,
            tool_id_canonicalized,
        })
    }

    fn extract_session_id_openai_chat(call: &LlmCall) -> Option<SessionIdExtraction> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let msgs = v.get("messages")?.as_array()?;
        let user_text = openai_chat::first_user_text(msgs)?;
        let sig = openai_chat::first_assistant_sig_from_request(msgs).or_else(|| {
            call.response_body
                .as_deref()
                .and_then(openai_chat::first_assistant_sig_from_response)
        })?;
        let (session_id, tool_id_canonicalized) = compose_session_id_tracked(&user_text, sig);
        Some(SessionIdExtraction {
            session_id,
            tool_id_canonicalized,
        })
    }

    fn extract_session_id_openai_responses(call: &LlmCall) -> Option<SessionIdExtraction> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        // input may be array (full mode) or string (simplified mode).
        let (user_text, sig_from_input) = match v.get("input")? {
            serde_json::Value::Array(items) => (
                openai_responses::first_user_text(items),
                openai_responses::first_assistant_sig_from_input(items),
            ),
            serde_json::Value::String(s) if !s.trim().is_empty() => (Some(s.clone()), None),
            _ => (None, None),
        };
        let user_text = user_text?;
        let sig = sig_from_input.or_else(|| {
            call.response_body
                .as_deref()
                .and_then(openai_responses::first_assistant_sig_from_response)
        })?;
        let (session_id, tool_id_canonicalized) = compose_session_id_tracked(&user_text, sig);
        Some(SessionIdExtraction {
            session_id,
            tool_id_canonicalized,
        })
    }

    fn is_user_turn_start_anthropic(call: &LlmCall) -> Option<bool> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        match last.get("content")? {
            serde_json::Value::String(s) => Some(!s.trim().is_empty()),
            serde_json::Value::Array(blocks) => Some(blocks.iter().any(|b| {
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

    fn is_user_turn_start_openai_chat(call: &LlmCall) -> Option<bool> {
        // Last message is role=user with non-empty content. (User content blocks
        // in Chat Completions don't have a `tool_result` type — tool results
        // are role=tool messages, not role=user.)
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        Some(
            last.get("content")
                .and_then(openai_chat::user_content_to_text)
                .is_some(),
        )
    }

    fn is_user_turn_start_openai_responses(call: &LlmCall) -> Option<bool> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let items = match v.get("input")? {
            serde_json::Value::Array(items) => items,
            serde_json::Value::String(s) => return Some(!s.trim().is_empty()),
            _ => return None,
        };
        let last = items.last()?;
        let t = last.get("type").and_then(|v| v.as_str());
        if t != Some("message") || last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        Some(openai_responses::message_text(last).is_some())
    }

    fn extract_user_input_anthropic(call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return None;
        }
        let raw = match last.get("content")? {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => {
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

    fn extract_user_input_openai_chat(call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return None;
        }
        last.get("content").and_then(openai_chat::user_content_to_text)
    }

    fn extract_user_input_openai_responses(call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        match v.get("input")? {
            serde_json::Value::Array(items) => {
                for it in items.iter().rev() {
                    if it.get("type").and_then(|v| v.as_str()) != Some("message") {
                        continue;
                    }
                    if it.get("role").and_then(|v| v.as_str()) != Some("user") {
                        continue;
                    }
                    if let Some(t) = openai_responses::message_text(it) {
                        return Some(t);
                    }
                }
                None
            }
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    fn extract_assistant_text_anthropic(call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
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

    fn extract_assistant_text_openai_chat(call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let c = v
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

    fn extract_assistant_text_openai_responses(call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let output = v.get("output")?.as_array()?;
        for it in output {
            if it.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            if let Some(t) = openai_responses::message_text(it) {
                return Some(t);
            }
        }
        None
    }
}

impl AgentProfile for GenericProfile {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        matches!(
            call.wire_api,
            wa::ANTHROPIC | wa::OPENAI_CHAT | wa::OPENAI_RESPONSES
        )
    }

    fn extract_session_id(&self, call: &LlmCall) -> Option<SessionIdExtraction> {
        match call.wire_api {
            wa::ANTHROPIC => Self::extract_session_id_anthropic(call),
            wa::OPENAI_CHAT => Self::extract_session_id_openai_chat(call),
            wa::OPENAI_RESPONSES => Self::extract_session_id_openai_responses(call),
            _ => None,
        }
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        match call.wire_api {
            wa::ANTHROPIC => Self::is_user_turn_start_anthropic(call),
            wa::OPENAI_CHAT => Self::is_user_turn_start_openai_chat(call),
            wa::OPENAI_RESPONSES => Self::is_user_turn_start_openai_responses(call),
            _ => None,
        }
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        match call.wire_api {
            wa::ANTHROPIC => Self::extract_user_input_anthropic(call),
            wa::OPENAI_CHAT => Self::extract_user_input_openai_chat(call),
            wa::OPENAI_RESPONSES => Self::extract_user_input_openai_responses(call),
            _ => None,
        }
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        match call.wire_api {
            wa::ANTHROPIC => Self::extract_assistant_text_anthropic(call),
            wa::OPENAI_CHAT => Self::extract_assistant_text_openai_chat(call),
            wa::OPENAI_RESPONSES => Self::extract_assistant_text_openai_responses(call),
            _ => None,
        }
    }

    fn is_turn_terminal(&self, call: &LlmCall, wire_apis: &WireApiRegistry) -> bool {
        // OpenAI Responses' wire-api `status: "completed"` is unreliable
        // (always present even on tool-roundtrip pending), so inspect the
        // response body directly — same reasoning as `CodexCliProfile`.
        // Anthropic and OpenAI Chat fall through to the trait-default
        // implicit-path dispatch (duplicated here because traits have no
        // `super` to call).
        if call.wire_api == wa::OPENAI_RESPONSES {
            crate::wire_apis::openai::body_has_terminal_message_only(call.response_body.as_deref())
        } else {
            let Some(reason) = call.finish_reason.as_deref() else {
                return false;
            };
            let Some(api) = wire_apis.find_by_name(call.wire_api) else {
                return false;
            };
            api.is_terminal(reason) && !api.is_tool_use(reason)
        }
    }
}

// ─────────────────────────────── Tests ──────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        req: Option<&str>,
        resp: Option<&str>,
    ) -> LlmCall {
        let path = match wire_api {
            wa::ANTHROPIC => "/v1/messages",
            wa::OPENAI_CHAT => "/v1/chat/completions",
            wa::OPENAI_RESPONSES => "/v1/responses",
            _ => "/",
        };
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "m".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: path.into(),
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
            request_headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            response_headers: vec![],
        }
    }

    // ── Cross-wire-api: matches() ───────────────────────────────────────────

    #[test]
    fn matches_all_three_wire_apis() {
        for &wire in &[wa::ANTHROPIC, wa::OPENAI_CHAT, wa::OPENAI_RESPONSES] {
            let c = call_with(wire, vec![], None, None);
            assert!(GenericProfile.matches(&c), "should match {wire}");
        }
    }

    // ───────────────────── Anthropic (wire_api=anthropic) ──────────────────
    mod anthropic_wire {
        use super::*;

        fn ant(headers: Vec<(&str, &str)>, req: Option<&str>, resp: Option<&str>) -> LlmCall {
            call_with(wa::ANTHROPIC, headers, req, resp)
        }

        #[test]
        fn extract_session_id_call_n_with_tool_history() {
            let req = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"ok"}]}
            ]}"#;
            let c = ant(vec![], Some(req), None);
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert_eq!(ids.session_id, "toolu_abc");
        }

        #[test]
        fn extract_session_id_call_n_with_text_only_history() {
            let req = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[{"type":"text","text":"hello there"}]},
                {"role":"user","content":[{"type":"text","text":"more"}]}
            ]}"#;
            let c = ant(vec![], Some(req), None);
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
            assert_eq!(ids.session_id.len(), "gen-".len() + 16);
        }

        #[test]
        fn extract_session_id_call_1_tool_in_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let resp = r#"{"content":[{"type":"tool_use","id":"toolu_xyz","name":"Read","input":{}}]}"#;
            let c = ant(vec![], Some(req), Some(resp));
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert_eq!(ids.session_id, "toolu_xyz");
        }

        #[test]
        fn extract_session_id_call_1_text_in_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let resp = r#"{"content":[{"type":"text","text":"hello there"}]}"#;
            let c = ant(vec![], Some(req), Some(resp));
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_call_1_and_n_match() {
            let resp =
                r#"{"content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]}"#;
            let req1 = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"prompt"}]}]}"#;
            let req2 = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"prompt"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_same","content":"ok"}]}
            ]}"#;
            let c1 = ant(vec![], Some(req1), Some(resp));
            let c2 = ant(vec![], Some(req2), None);
            let id1 = GenericProfile.extract_session_id(&c1).unwrap().session_id;
            let id2 = GenericProfile.extract_session_id(&c2).unwrap().session_id;
            assert_eq!(id1, id2, "call #1 and call #2 must synthesize same session_id");
        }

        #[test]
        fn extract_session_id_call_1_with_normalized_tool_id() {
            let resp = r#"{"content":[{"type":"tool_use","id":"toolu_abc","name":"R","input":{}}]}"#;
            let req2 = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"x"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"tooluabc","name":"R","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"tooluabc","content":"ok"}]}
            ]}"#;
            let c1 = ant(
                vec![],
                Some(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#),
                Some(resp),
            );
            let c2 = ant(vec![], Some(req2), None);
            assert_eq!(
                GenericProfile.extract_session_id(&c1).unwrap().session_id,
                GenericProfile.extract_session_id(&c2).unwrap().session_id,
            );
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = ant(vec![], Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn is_user_turn_start_text() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert_eq!(GenericProfile.is_user_turn_start(&c), Some(true));
        }

        #[test]
        fn is_user_turn_start_tool_result_only() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert_eq!(GenericProfile.is_user_turn_start(&c), Some(false));
        }
    }

    // ─────────────────── OpenAI Chat (wire_api=openai-chat) ────────────────
    mod openai_chat_wire {
        use super::*;

        fn oai(req: Option<&str>, resp: Option<&str>) -> LlmCall {
            call_with(wa::OPENAI_CHAT, vec![], req, resp)
        }

        #[test]
        fn extract_session_id_call_n_with_tool_history() {
            let req = r#"{"messages":[
                {"role":"system","content":"you are helpful"},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_abc","content":"ok"}
            ]}"#;
            let c = oai(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert_eq!(ids.session_id, "call_abc");
        }

        #[test]
        fn extract_session_id_call_1_tool_in_response_canonicalized() {
            // OpenClaw quirk: tool_id without underscore in echo, canonical in response.
            let req1 = r#"{"messages":[{"role":"user","content":"x"}]}"#;
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#;
            let req2 = r#"{"messages":[
                {"role":"user","content":"x"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"callabc","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"callabc","content":"ok"}
            ]}"#;
            let c1 = oai(Some(req1), Some(resp));
            let c2 = oai(Some(req2), None);
            let id1 = GenericProfile.extract_session_id(&c1).unwrap().session_id;
            let id2 = GenericProfile.extract_session_id(&c2).unwrap().session_id;
            assert_eq!(id1, "call_abc");
            assert_eq!(id1, id2, "canonicalized form must match across calls");
        }

        #[test]
        fn extract_session_id_call_n_with_text_only_history() {
            let req = r#"{"messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello"},
                {"role":"user","content":"more"}
            ]}"#;
            let c = oai(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_call_1_text_in_response() {
            let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
            let c = oai(Some(req), Some(resp));
            assert!(GenericProfile
                .extract_session_id(&c)
                .unwrap()
                .session_id
                .starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
            let c = oai(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = oai(Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn first_user_skips_system() {
            let req = r#"{"messages":[
                {"role":"system","content":"you are X"},
                {"role":"user","content":"actual prompt"}
            ]}"#;
            let v: serde_json::Value = serde_json::from_str(req).unwrap();
            let msgs = v.get("messages").unwrap().as_array().unwrap();
            assert_eq!(
                openai_chat::first_user_text(msgs).as_deref(),
                Some("actual prompt"),
            );
        }

        #[test]
        fn is_user_turn_start_text() {
            let req = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&oai(Some(req), None)),
                Some(true),
            );
        }

        #[test]
        fn is_user_turn_start_false_when_last_is_tool() {
            let req = r#"{"messages":[
                {"role":"user","content":"x"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_a","content":"ok"}
            ]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&oai(Some(req), None)),
                Some(false),
            );
        }
    }

    // ─────────────── OpenAI Responses (wire_api=openai-responses) ──────────
    mod openai_responses_wire {
        use super::*;

        fn resp(req: Option<&str>, resp: Option<&str>) -> LlmCall {
            call_with(wa::OPENAI_RESPONSES, vec![], req, resp)
        }

        #[test]
        fn extract_session_id_call_n_with_function_call() {
            let req = r#"{"input":[
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"sys"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"reasoning","summary":[],"content":[]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_xyz"}
            ]}"#;
            let c = resp(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert_eq!(
                ids.session_id, "fc_xyz",
                "function_call.call_id wins over assistant text"
            );
        }

        #[test]
        fn extract_session_id_call_1_function_call_in_response() {
            let req =
                r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
            let r =
                r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_abc"}]}"#;
            let c = resp(Some(req), Some(r));
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert_eq!(ids.session_id, "fc_abc");
        }

        #[test]
        fn extract_session_id_call_1_and_n_match() {
            let req1 = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]}]}"#;
            let r =
                r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"}]}"#;
            let req2 = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"},
                {"type":"function_call_output","call_id":"fc_same","output":"ok"}
            ]}"#;
            let c1 = resp(Some(req1), Some(r));
            let c2 = resp(Some(req2), None);
            assert_eq!(
                GenericProfile.extract_session_id(&c1).unwrap().session_id,
                GenericProfile.extract_session_id(&c2).unwrap().session_id,
            );
        }

        #[test]
        fn extract_session_id_call_n_with_text_only_assistant() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}
            ]}"#;
            let c = resp(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_input_string_mode_treats_as_call_1() {
            let req = r#"{"input":"just a prompt"}"#;
            let r =
                r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_simple"}]}"#;
            let c = resp(Some(req), Some(r));
            assert_eq!(
                GenericProfile.extract_session_id(&c).unwrap().session_id,
                "fc_simple",
            );
        }

        #[test]
        fn extract_session_id_none_when_string_input_and_no_response() {
            let req = r#"{"input":"just a prompt"}"#;
            let c = resp(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
            let c = resp(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = resp(Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn is_turn_terminal_delegates_to_helper() {
            let r = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}"#;
            let mut c = resp(None, Some(r));
            let wires = crate::wire_apis::build_default_wire_api_registry();
            assert!(GenericProfile.is_turn_terminal(&c, &wires));
            c.response_body = Some(
                r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#
                    .to_string(),
            );
            assert!(!GenericProfile.is_turn_terminal(&c, &wires));
        }

        #[test]
        fn is_user_turn_start_last_user_message() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
                {"type":"function_call_output","call_id":"fc_a","output":"ok"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"more"}]}
            ]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&resp(Some(req), None)),
                Some(true),
            );
        }

        #[test]
        fn is_user_turn_start_false_when_last_is_function_call_output() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
                {"type":"function_call_output","call_id":"fc_a","output":"ok"}
            ]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&resp(Some(req), None)),
                Some(false),
            );
        }

        #[test]
        fn extract_assistant_text_returns_first_message_text() {
            let r = r#"{"output":[
                {"type":"reasoning","summary":[],"content":[]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"the answer"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}
            ]}"#;
            let c = resp(None, Some(r));
            assert_eq!(
                GenericProfile.extract_assistant_text(&c).as_deref(),
                Some("the answer"),
            );
        }

        #[test]
        fn extract_assistant_text_none_when_no_message_item() {
            let r = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#;
            let c = resp(None, Some(r));
            assert_eq!(GenericProfile.extract_assistant_text(&c), None);
        }
    }
}
