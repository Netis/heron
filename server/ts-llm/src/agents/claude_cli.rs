use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_apis as wa;
use serde_json::Value;

pub struct ClaudeCliProfile;

const SESSION_HEADER: &str = "x-claude-code-session-id";
const UA_PREFIX: &str = "claude-cli/";

fn header<'a>(call: &'a LlmCall, key: &str) -> Option<&'a str> {
    call.request_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// Parse the `tools` array from the request body. `None` when the field is
/// absent or the body is not valid JSON; `Some(vec)` when the field is
/// present (possibly empty).
fn parse_tools(body: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(body).ok()?;
    let arr = v.get("tools")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect(),
    )
}

/// A call is a Task sub-agent invocation when `tools` is non-empty but does
/// not include the `"Agent"` tool. claude-cli forbids sub-agents from
/// spawning further sub-agents, so `Agent` presence is the hard structural
/// marker for a main-agent request.
fn looks_like_subagent(body: &str) -> bool {
    match parse_tools(body) {
        Some(tools) => !tools.is_empty() && !tools.iter().any(|n| n == "Agent"),
        None => false,
    }
}

/// Remove `<system-reminder>...</system-reminder>` blocks (Claude Code scaffolding)
/// from a user text block. Non-greedy across lines.
fn strip_system_reminders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<system-reminder>") {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(end) = tail.find("</system-reminder>") {
            rest = &tail[end + "</system-reminder>".len()..];
        } else {
            // Unterminated — drop the remainder to avoid leaking scaffolding.
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

impl AgentProfile for ClaudeCliProfile {
    fn name(&self) -> &'static str {
        "claude-cli"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        if call.wire_api != wa::ANTHROPIC {
            return false;
        }
        header(call, "user-agent")
            .map(|ua| ua.starts_with(UA_PREFIX))
            .unwrap_or(false)
    }

    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
        let session_id = header(call, SESSION_HEADER)?.to_string();
        // Anthropic: no protocol-level turn_id; tracker will generate it.
        Some(ExtractedIds {
            session_id,
            turn_id: None,
        })
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let msgs = v.get("messages")?.as_array()?;
        let last = msgs.last()?;
        if last.get("role")?.as_str()? != "user" {
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
        let cleaned = strip_system_reminders(&raw);
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let content = v.get("content")?.as_array()?;
        let text: String = content
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn subagent(&self, call: &LlmCall) -> Option<String> {
        // Structural marker: `tools` is non-empty but doesn't include "Agent".
        // claude-cli doesn't expose a sub-agent name over the wire, so we use
        // a placeholder tag. The tracker only cares whether this is `Some`.
        let body = call.request_body.as_deref()?;
        if looks_like_subagent(body) {
            Some("task".to_string())
        } else {
            None
        }
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        // Reuse extract_user_input: it already strips <system-reminder> blocks
        // and trims whitespace, so Claude CLI compaction requests (whose last
        // user message contains only a system-reminder summary) collapse to
        // None and are correctly treated as continuations, not new turns.
        let body = call.request_body.as_deref()?;
        // Sub-agent Task invocations carry fresh user text but belong to the
        // parent main-agent turn; they must not open a new turn.
        if looks_like_subagent(body) {
            return Some(false);
        }
        Some(self.extract_user_input(call).is_some())
    }

    fn is_auxiliary(&self, call: &LlmCall) -> bool {
        let Some(body) = call.request_body.as_deref() else {
            return false;
        };
        // Auxiliary = non-agentic one-shot (e.g., session-title generation):
        // `tools` field explicitly present and empty. A missing `tools` field
        // is ambiguous (could be a test fixture or a legitimate non-agentic
        // call) and is treated conservatively as non-auxiliary.
        match parse_tools(body) {
            Some(tools) => tools.is_empty(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        body: Option<&str>,
    ) -> LlmCall {
        LlmCall {
            stream_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "claude".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/messages".into(),
            is_stream: true,
            request_body: body.map(str::to_string),
            status_code: None,
            finish_reason: None,
            response_body: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: None,
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

    #[test]
    fn matches_anthropic_claude_cli_user_agent() {
        let c = call_with(
            wa::ANTHROPIC,
            vec![("User-Agent", "claude-cli/2.1.98 (external, cli)")],
            None,
        );
        assert!(ClaudeCliProfile.matches(&c));
    }

    #[test]
    fn does_not_match_other_wire_api() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "claude-cli/2.1.98 (external, cli)")],
            None,
        );
        assert!(!ClaudeCliProfile.matches(&c));
    }

    #[test]
    fn does_not_match_other_user_agent() {
        let c = call_with(wa::ANTHROPIC, vec![("User-Agent", "curl/8.1.2")], None);
        assert!(!ClaudeCliProfile.matches(&c));
    }

    #[test]
    fn extract_ids_returns_session_from_header() {
        let c = call_with(
            wa::ANTHROPIC,
            vec![
                ("User-Agent", "claude-cli/2.1.98"),
                (
                    "X-Claude-Code-Session-Id",
                    "7dd4ea24-82c9-4035-afa1-89f6b2c742b9",
                ),
            ],
            None,
        );
        let ids = ClaudeCliProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "7dd4ea24-82c9-4035-afa1-89f6b2c742b9");
        assert!(ids.turn_id.is_none());
    }

    #[test]
    fn extract_ids_none_when_session_header_missing() {
        let c = call_with(
            wa::ANTHROPIC,
            vec![("User-Agent", "claude-cli/2.1.98")],
            None,
        );
        assert!(ClaudeCliProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn is_user_turn_start_text_content() {
        let body = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"help me"}]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c), Some(true));
    }

    #[test]
    fn is_user_turn_start_tool_result_only() {
        let body = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c), Some(false));
    }

    #[test]
    fn is_user_turn_start_string_content() {
        let body = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c), Some(true));
    }

    #[test]
    fn is_user_turn_start_mixed_text_and_tool_result_counts_as_user() {
        let body = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"},{"type":"text","text":"also, stop"}]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c), Some(true));
    }

    #[test]
    fn is_user_turn_start_none_when_no_body() {
        let c = call_with(wa::ANTHROPIC, vec![], None);
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c), None);
    }

    #[test]
    fn is_user_turn_start_false_for_subagent_with_user_text() {
        // Sub-agent: tools non-empty but no "Agent" tool → carry user text but
        // must not open a new main-agent turn.
        let body = r#"{
            "messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],
            "tools":[{"name":"Read"},{"name":"Grep"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c), Some(false));
    }

    #[test]
    fn is_user_turn_start_true_for_main_agent_with_user_text() {
        // Main agent: tools include "Agent" → fresh user text opens new turn.
        let body = r#"{
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
            "tools":[{"name":"Agent"},{"name":"Bash"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c), Some(true));
    }

    #[test]
    fn is_auxiliary_true_when_tools_empty() {
        // Title-gen style one-shot: no tools → auxiliary.
        let body = r#"{
            "messages":[{"role":"user","content":"generate title"}],
            "tools":[]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(ClaudeCliProfile.is_auxiliary(&c));
    }

    #[test]
    fn is_auxiliary_false_when_tools_field_missing() {
        // Ambiguous: could be legacy/test fixture. Conservative = not aux.
        let body = r#"{"messages":[{"role":"user","content":"x"}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(!ClaudeCliProfile.is_auxiliary(&c));
    }

    #[test]
    fn is_auxiliary_false_for_main_agent() {
        let body = r#"{
            "messages":[{"role":"user","content":"x"}],
            "tools":[{"name":"Agent"},{"name":"Bash"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(!ClaudeCliProfile.is_auxiliary(&c));
    }

    #[test]
    fn is_auxiliary_false_for_subagent() {
        let body = r#"{
            "messages":[{"role":"user","content":"x"}],
            "tools":[{"name":"Read"},{"name":"Grep"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(!ClaudeCliProfile.is_auxiliary(&c));
    }

    #[test]
    fn is_auxiliary_false_when_body_missing() {
        let c = call_with(wa::ANTHROPIC, vec![], None);
        assert!(!ClaudeCliProfile.is_auxiliary(&c));
    }

    #[test]
    fn strip_system_reminders_removes_blocks() {
        let s = "hello <system-reminder>internal\nnote</system-reminder> world";
        assert_eq!(strip_system_reminders(s), "hello  world");
    }

    #[test]
    fn strip_system_reminders_handles_multiple() {
        let s = "<system-reminder>a</system-reminder>x<system-reminder>b</system-reminder>y";
        assert_eq!(strip_system_reminders(s), "xy");
    }

    #[test]
    fn extract_user_input_concatenates_text_blocks() {
        let body = r#"{"messages":[{"role":"user","content":[
            {"type":"tool_result","content":"ignored"},
            {"type":"text","text":"hello"},
            {"type":"text","text":"world"}
        ]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(
            ClaudeCliProfile.extract_user_input(&c).as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn extract_user_input_strips_system_reminder() {
        let body = r#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"<system-reminder>do not mention this</system-reminder>actual question"}
        ]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(
            ClaudeCliProfile.extract_user_input(&c).as_deref(),
            Some("actual question")
        );
    }

    #[test]
    fn extract_user_input_string_content() {
        let body = r#"{"messages":[{"role":"user","content":"plain prompt"}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(
            ClaudeCliProfile.extract_user_input(&c).as_deref(),
            Some("plain prompt")
        );
    }

    #[test]
    fn extract_user_input_none_when_tool_result_only() {
        let body = r#"{"messages":[{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t","content":"ok"}
        ]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.extract_user_input(&c), None);
    }

    #[test]
    fn extract_assistant_text_joins_text_blocks_only() {
        let body = r#"{"content":[
            {"type":"thinking","thinking":"internal"},
            {"type":"text","text":"part one"},
            {"type":"tool_use","id":"t","name":"bash","input":{}},
            {"type":"text","text":"part two"}
        ]}"#;
        let mut c = call_with(wa::ANTHROPIC, vec![], None);
        c.response_body = Some(body.to_string());
        assert_eq!(
            ClaudeCliProfile.extract_assistant_text(&c).as_deref(),
            Some("part one\npart two")
        );
    }

    #[test]
    fn extract_assistant_text_none_when_no_text() {
        let body = r#"{"content":[{"type":"tool_use","id":"t","name":"bash","input":{}}]}"#;
        let mut c = call_with(wa::ANTHROPIC, vec![], None);
        c.response_body = Some(body.to_string());
        assert_eq!(ClaudeCliProfile.extract_assistant_text(&c), None);
    }
}
