use crate::model::LlmCall;
use crate::profile::{AgentProfile, CallCtx, SessionIdExtraction};
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
/// absent; `Some(vec)` when the field is present (possibly empty).
fn parse_tools(req: &Value) -> Option<Vec<String>> {
    let arr = req.get("tools")?.as_array()?;
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
fn looks_like_subagent(req: &Value) -> bool {
    match parse_tools(req) {
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

    fn matches(&self, ctx: &CallCtx<'_>) -> bool {
        if ctx.call.wire_api != wa::ANTHROPIC {
            return false;
        }
        header(ctx.call, "user-agent")
            .map(|ua| ua.starts_with(UA_PREFIX))
            .unwrap_or(false)
    }

    fn extract_session_id(&self, ctx: &CallCtx<'_>) -> Option<SessionIdExtraction> {
        let session_id = header(ctx.call, SESSION_HEADER)?.to_string();
        Some(SessionIdExtraction {
            session_id,
            tool_id_canonicalized: false,
        })
    }

    fn extract_user_input(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let req = ctx.req?;
        let msgs = req.get("messages")?.as_array()?;
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

    fn extract_assistant_text(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let resp = ctx.resp?;
        let content = resp.get("content")?.as_array()?;
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

    fn subagent(&self, ctx: &CallCtx<'_>) -> Option<String> {
        // Structural marker: `tools` is non-empty but doesn't include "Agent".
        // claude-cli doesn't expose a sub-agent name over the wire, so we use
        // a placeholder tag. The tracker only cares whether this is `Some`.
        let req = ctx.req?;
        if looks_like_subagent(req) {
            Some("task".to_string())
        } else {
            None
        }
    }

    fn is_user_turn_start(&self, ctx: &CallCtx<'_>) -> Option<bool> {
        // Structural: last message is `role=user` AND its content contains at
        // least one user-visible block. "User-visible" means:
        //   - tool_result blocks → DON'T count (continuation of a prior
        //     assistant tool_use)
        //   - text blocks → only count if non-empty after stripping
        //     `<system-reminder>` wrapping (Claude CLI compaction requests
        //     send a system-reminder-only text block which is a continuation,
        //     not a new turn)
        //   - any other block type (image, future block types) → count as
        //     user-visible (this is the future-proof bit; the previous impl
        //     binding to `extract_user_input.is_some()` misclassified
        //     image-only messages as continuations)
        //
        // Decoupled from `extract_user_input`: that one is a preview
        // extractor for text only; this one is the structural turn-start
        // predicate. Sub-agent filtering happens at the ts-llm stage.
        let req = ctx.req?;
        let last = req.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        match last.get("content")? {
            Value::String(s) => Some(!strip_system_reminders(s).trim().is_empty()),
            Value::Array(blocks) => {
                Some(
                    blocks
                        .iter()
                        .any(|b| match b.get("type").and_then(|t| t.as_str()) {
                            Some("tool_result") => false,
                            Some("text") => {
                                let t = b.get("text").and_then(|x| x.as_str()).unwrap_or("");
                                !strip_system_reminders(t).trim().is_empty()
                            }
                            Some(_) => true,
                            None => false,
                        }),
                )
            }
            _ => None,
        }
    }

    fn is_auxiliary(&self, ctx: &CallCtx<'_>) -> bool {
        let Some(req) = ctx.req else {
            return false;
        };
        // Auxiliary = non-agentic one-shot (e.g., session-title generation):
        // `tools` field explicitly present and empty. A missing `tools` field
        // is ambiguous (could be a test fixture or a legitimate non-agentic
        // call) and is treated conservatively as non-auxiliary.
        match parse_tools(req) {
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
    ) -> crate::profile::TestCall {
        crate::profile::TestCall::new(LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "claude".into(),
            api_type: ApiType::Chat,
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
        })
    }

    #[test]
    fn matches_anthropic_claude_cli_user_agent() {
        let c = call_with(
            wa::ANTHROPIC,
            vec![("User-Agent", "claude-cli/2.1.98 (external, cli)")],
            None,
        );
        assert!(ClaudeCliProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_other_wire_api() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "claude-cli/2.1.98 (external, cli)")],
            None,
        );
        assert!(!ClaudeCliProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_other_user_agent() {
        let c = call_with(wa::ANTHROPIC, vec![("User-Agent", "curl/8.1.2")], None);
        assert!(!ClaudeCliProfile.matches(&c.ctx()));
    }

    #[test]
    fn extract_session_id_returns_session_from_header() {
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
        let ids = ClaudeCliProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, "7dd4ea24-82c9-4035-afa1-89f6b2c742b9");
    }

    #[test]
    fn extract_session_id_none_when_session_header_missing() {
        let c = call_with(
            wa::ANTHROPIC,
            vec![("User-Agent", "claude-cli/2.1.98")],
            None,
        );
        assert!(ClaudeCliProfile.extract_session_id(&c.ctx()).is_none());
    }

    #[test]
    fn is_user_turn_start_text_content() {
        let body = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"help me"}]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(true));
    }

    #[test]
    fn is_user_turn_start_tool_result_only() {
        let body = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(false));
    }

    #[test]
    fn is_user_turn_start_string_content() {
        let body = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(true));
    }

    #[test]
    fn is_user_turn_start_mixed_text_and_tool_result_counts_as_user() {
        let body = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"},{"type":"text","text":"also, stop"}]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(true));
    }

    #[test]
    fn is_user_turn_start_none_when_no_body() {
        let c = call_with(wa::ANTHROPIC, vec![], None);
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), None);
    }

    #[test]
    fn is_user_turn_start_returns_raw_structural_for_subagent_body() {
        // Sub-agent dispatch carries fresh user text. The profile predicate is
        // structural and answers Some(true) here; sub-agent filtering happens
        // at the ts-llm stage by combining `subagent_name` with this verdict.
        let body = r#"{
            "messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],
            "tools":[{"name":"Read"},{"name":"Grep"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(true));
        // And the call is correctly tagged as sub-agent.
        assert_eq!(
            ClaudeCliProfile.subagent(&c.ctx()),
            Some("task".to_string())
        );
    }

    #[test]
    fn is_user_turn_start_true_for_main_agent_with_user_text() {
        // Main agent: tools include "Agent" → fresh user text opens new turn.
        let body = r#"{
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
            "tools":[{"name":"Agent"},{"name":"Bash"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(true));
    }

    #[test]
    fn is_user_turn_start_true_for_image_only_user_message() {
        // Image-only user message: no text blocks at all. Structural check
        // must still recognize this as a fresh user turn — `extract_user_input`
        // returns None (it filters to text), but turn-start IS Some(true).
        // Decoupling: `is_user_turn_start` is structural, `extract_user_input`
        // is text preview; they can disagree on non-text user input.
        let body = r#"{
            "messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBOR"}}]}],
            "tools":[{"name":"Agent"},{"name":"Bash"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(true));
        assert!(
            ClaudeCliProfile.extract_user_input(&c.ctx()).is_none(),
            "extract_user_input is text-only by design; image bodies have no text preview"
        );
    }

    #[test]
    fn is_user_turn_start_false_for_system_reminder_only_compaction() {
        // Claude CLI compaction: last user message is a single text block whose
        // content is wrapped entirely in <system-reminder>. After stripping,
        // the text is empty → not a fresh user turn (continuation).
        let body = r#"{
            "messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>summary of prior turn</system-reminder>"}]}],
            "tools":[{"name":"Agent"},{"name":"Bash"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.is_user_turn_start(&c.ctx()), Some(false));
    }

    #[test]
    fn is_auxiliary_true_when_tools_empty() {
        // Title-gen style one-shot: no tools → auxiliary.
        let body = r#"{
            "messages":[{"role":"user","content":"generate title"}],
            "tools":[]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(ClaudeCliProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_false_when_tools_field_missing() {
        // Ambiguous: could be legacy/test fixture. Conservative = not aux.
        let body = r#"{"messages":[{"role":"user","content":"x"}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(!ClaudeCliProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_false_for_main_agent() {
        let body = r#"{
            "messages":[{"role":"user","content":"x"}],
            "tools":[{"name":"Agent"},{"name":"Bash"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(!ClaudeCliProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_false_for_subagent() {
        let body = r#"{
            "messages":[{"role":"user","content":"x"}],
            "tools":[{"name":"Read"},{"name":"Grep"}]
        }"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert!(!ClaudeCliProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_false_when_body_missing() {
        let c = call_with(wa::ANTHROPIC, vec![], None);
        assert!(!ClaudeCliProfile.is_auxiliary(&c.ctx()));
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
            ClaudeCliProfile.extract_user_input(&c.ctx()).as_deref(),
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
            ClaudeCliProfile.extract_user_input(&c.ctx()).as_deref(),
            Some("actual question")
        );
    }

    #[test]
    fn extract_user_input_string_content() {
        let body = r#"{"messages":[{"role":"user","content":"plain prompt"}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(
            ClaudeCliProfile.extract_user_input(&c.ctx()).as_deref(),
            Some("plain prompt")
        );
    }

    #[test]
    fn extract_user_input_none_when_tool_result_only() {
        let body = r#"{"messages":[{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t","content":"ok"}
        ]}]}"#;
        let c = call_with(wa::ANTHROPIC, vec![], Some(body));
        assert_eq!(ClaudeCliProfile.extract_user_input(&c.ctx()), None);
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
        c.set_response_body(body);
        assert_eq!(
            ClaudeCliProfile.extract_assistant_text(&c.ctx()).as_deref(),
            Some("part one\npart two")
        );
    }

    #[test]
    fn extract_assistant_text_none_when_no_text() {
        let body = r#"{"content":[{"type":"tool_use","id":"t","name":"bash","input":{}}]}"#;
        let mut c = call_with(wa::ANTHROPIC, vec![], None);
        c.set_response_body(body);
        assert_eq!(ClaudeCliProfile.extract_assistant_text(&c.ctx()), None);
    }
}
