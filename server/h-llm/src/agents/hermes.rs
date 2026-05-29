//! Hermes profile — Nous Research's Hermes Agent. Hermes uses the upstream
//! `openai-python` / Anthropic SDKs verbatim with no client-specific headers
//! (UA is `OpenAI/Python <ver>` or similar), so identification is body
//! fingerprint only:
//!
//!   `tools[]` contains ≥2 of Hermes's distinctive tool names
//!   (`skill_view`, `skill_manage`, `skills_list`, `delegate_task`,
//!   `session_search`, `cronjob`).
//!
//! Markers were chosen to be unique to Hermes's runtime surface — its
//! skill system (`skill_*`/`skills_list`), sub-agent dispatch
//! (`delegate_task`), cross-session memory (`session_search`), and
//! scheduling (`cronjob`). Generic-name tools that overlap with other
//! agents (`terminal`, `read_file`, `write_file`, `memory`, `todo`,
//! `process`, `patch`, `execute_code`, `browser_*`) are deliberately
//! excluded to avoid false positives.
//!
//! Hermes's chat-title-generation call (system: "Generate a short,
//! descriptive title…") carries no `tools` and no Hermes markers; it does
//! NOT match this profile. Generic fallback also ignores text-only calls, so
//! title generation remains visible as an LLM call without creating a
//! synthetic agent turn.

use crate::agent_primitives::{AgentPrimitives, SystemPromptMarkers};
use crate::profile::{AgentProfile, CallCtx, SessionIdExtraction};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;
use serde_json::Value;

use super::session_id::compose_session_id_tracked;

pub struct HermesProfile;

const AGENT_NAME: &str = "hermes";

const MARKER_TOOLS: &[&str] = &[
    "skill_view",
    "skill_manage",
    "skills_list",
    "delegate_task",
    "session_search",
    "cronjob",
];
const MARKER_HITS_REQUIRED: usize = 2;

fn parse_tool_names(wire_api: &str, req: &Value) -> Option<Vec<String>> {
    match wire_api {
        wa::ANTHROPIC => wa::anthropic::tool_names(req),
        wa::OPENAI_CHAT => wa::openai::chat::tool_names(req),
        wa::OPENAI_RESPONSES => wa::openai::responses::tool_names(req),
        _ => None,
    }
}

impl AgentProfile for HermesProfile {
    fn name(&self) -> &'static str {
        AGENT_NAME
    }

    fn matches(&self, ctx: &CallCtx<'_>) -> bool {
        if !matches!(
            ctx.call.wire_api,
            wa::ANTHROPIC | wa::OPENAI_CHAT | wa::OPENAI_RESPONSES
        ) {
            return false;
        }
        let Some(req) = ctx.req else {
            return false;
        };
        let Some(names) = parse_tool_names(ctx.call.wire_api, req) else {
            return false;
        };
        let hits = MARKER_TOOLS
            .iter()
            .filter(|m| names.iter().any(|n| n == *m))
            .count();
        hits >= MARKER_HITS_REQUIRED
    }

    fn extract_session_id(&self, ctx: &CallCtx<'_>) -> Option<SessionIdExtraction> {
        let req = ctx.req?;
        let (user_text, sig) = match ctx.call.wire_api {
            wa::ANTHROPIC => {
                let user_text = wa::anthropic::first_user_text(req)?;
                let sig = wa::anthropic::first_assistant_sig_from_request(req).or_else(|| {
                    ctx.resp
                        .and_then(wa::anthropic::first_assistant_sig_from_response_value)
                })?;
                (user_text, sig)
            }
            wa::OPENAI_CHAT => {
                let user_text = wa::openai::chat::first_user_text(req)?;
                let sig =
                    wa::openai::chat::first_assistant_sig_from_request(req).or_else(|| {
                        ctx.resp
                            .and_then(wa::openai::chat::first_assistant_sig_from_response_value)
                    })?;
                (user_text, sig)
            }
            wa::OPENAI_RESPONSES => {
                let items = req.get("input")?.as_array()?;
                let user_text = wa::openai::responses::first_user_text(items)?;
                let sig =
                    wa::openai::responses::first_assistant_sig_from_input(items).or_else(|| {
                        ctx.resp.and_then(
                            wa::openai::responses::first_assistant_sig_from_response_value,
                        )
                    })?;
                (user_text, sig)
            }
            _ => return None,
        };
        let (session_id, tool_id_canonicalized) = compose_session_id_tracked(&user_text, sig);
        Some(SessionIdExtraction {
            session_id,
            tool_id_canonicalized,
        })
    }

    fn is_user_turn_start(&self, ctx: &CallCtx<'_>) -> Option<bool> {
        let req = ctx.req?;
        match ctx.call.wire_api {
            wa::ANTHROPIC => wa::anthropic::is_user_turn_start(req),
            wa::OPENAI_CHAT => wa::openai::chat::is_user_turn_start(req),
            wa::OPENAI_RESPONSES => wa::openai::responses::is_user_turn_start(req),
            _ => None,
        }
    }

    fn extract_user_input(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let req = ctx.req?;
        match ctx.call.wire_api {
            wa::ANTHROPIC => wa::anthropic::extract_user_input(req),
            wa::OPENAI_CHAT => wa::openai::chat::extract_user_input(req),
            wa::OPENAI_RESPONSES => wa::openai::responses::extract_user_input(req),
            _ => None,
        }
    }

    fn extract_assistant_text(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let resp = ctx.resp?;
        match ctx.call.wire_api {
            wa::ANTHROPIC => wa::anthropic::extract_assistant_text_value(resp),
            wa::OPENAI_CHAT => wa::openai::chat::extract_assistant_text_value(resp),
            wa::OPENAI_RESPONSES => wa::openai::responses::extract_assistant_text_value(resp),
            _ => None,
        }
    }

    fn is_turn_terminal(&self, ctx: &CallCtx<'_>, wire_apis: &WireApiRegistry) -> bool {
        // OpenAI Responses' wire-api `status: "completed"` is unreliable, so
        // inspect the response body directly — same reasoning as
        // `CodexCliProfile`, `OpenClawProfile`, `GenericProfile`. Anthropic
        // and OpenAI Chat fall through to the trait-default implicit-path
        // dispatch (duplicated here because traits have no `super` to call).
        if ctx.call.wire_api == wa::OPENAI_RESPONSES {
            match ctx.resp {
                Some(resp) => wa::openai::body_has_terminal_message_only_value(resp),
                None => false,
            }
        } else {
            let Some(reason) = ctx.call.finish_reason.as_deref() else {
                return false;
            };
            let Some(api) = wire_apis.find_by_name(ctx.call.wire_api) else {
                return false;
            };
            api.is_terminal(reason) && !api.is_tool_use(reason)
        }
    }

    // `subagent` left at trait default. Hermes ships `delegate_task` for
    // sub-agent dispatch; once captures of those calls are available the
    // override should fingerprint the dispatch's nested-prompt shape.
    // `is_auxiliary` left at trait default. Hermes's auxiliary calls
    // (title-gen, etc.) carry no Hermes markers, so they don't match this
    // profile in the first place — they fall through to GenericProfile.

    fn extract_primitives(&self, ctx: &CallCtx<'_>) -> AgentPrimitives {
        let mut p = AgentPrimitives::default();
        if let Some(req) = ctx.req {
            // Tool definitions in tools[].function.name (OpenAI Chat shape)
            // or tools[].name (Anthropic/Responses flat shape)
            if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
                for tool in tools {
                    let name = tool
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| tool.get("name").and_then(|n| n.as_str()));
                    if let Some(name) = name {
                        if !p.tool_names.iter().any(|n| n == name) {
                            p.tool_names.push(name.to_string());
                        }
                    }
                }
            }
            // Tool calls in messages[].tool_calls[] (OpenAI Chat)
            if let Some(messages) = req.get("messages").and_then(|m| m.as_array()) {
                for msg in messages {
                    if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                        for tc in tool_calls {
                            p.tool_call_count += 1;
                            if let Some(name) = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                            {
                                if !p.tool_names.iter().any(|n| n == name) {
                                    p.tool_names.push(name.to_string());
                                }
                            }
                        }
                    }
                    // System prompt markers
                    if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
                        if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                            if !content.is_empty() {
                                p.has_system_prompt = true;
                                let lower = content.to_lowercase();
                                if lower.contains("you are an agent")
                                    || lower.contains("you are claude code")
                                {
                                    p.system_prompt_markers |= SystemPromptMarkers::AGENT_LOOP;
                                }
                            }
                        }
                    }
                }
            }
        }
        p.subagent_marker = self.subagent(ctx);
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call(
        wire_api: &'static str,
        req: Option<&str>,
        resp: Option<&str>,
    ) -> crate::profile::TestCall {
        crate::profile::TestCall::new(LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "glm-5".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/".into(),
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
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
        })
    }

    /// OpenAI Chat Hermes body — the shape observed in hermes-openai.pcap:
    /// system carries the persona, user is plain text, tools carry the full
    /// Hermes runtime surface (we only include enough markers for the test).
    const HERMES_OAI_CHAT: &str = r##"{
      "messages":[
        {"role":"system","content":"# Hermes Agent Persona\n\nYou are a helpful assistant."},
        {"role":"user","content":"check status"}
      ],
      "tools":[
        {"type":"function","function":{"name":"terminal"}},
        {"type":"function","function":{"name":"read_file"}},
        {"type":"function","function":{"name":"skill_view"}},
        {"type":"function","function":{"name":"delegate_task"}},
        {"type":"function","function":{"name":"session_search"}}
      ]
    }"##;

    /// Anthropic Hermes body.
    const HERMES_ANTHROPIC: &str = r##"{
      "system":[{"type":"text","text":"# Hermes Agent Persona"}],
      "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
      "tools":[
        {"name":"terminal"},
        {"name":"skill_view"},
        {"name":"delegate_task"}
      ]
    }"##;

    /// OpenAI Responses Hermes body.
    const HERMES_RESPONSES: &str = r##"{
      "input":[
        {"type":"message","role":"developer","content":[{"type":"input_text","text":"# Hermes Agent Persona"}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
      ],
      "tools":[
        {"type":"function","name":"terminal","parameters":{}},
        {"type":"function","name":"skill_view","parameters":{}},
        {"type":"function","name":"delegate_task","parameters":{}}
      ]
    }"##;

    /// Title-generation body (no tools, generic title prompt). Verbatim
    /// shape from hermes-openai.pcap call#11.
    const HERMES_TITLE_GEN: &str = r#"{
      "messages":[
        {"role":"system","content":"Generate a short, descriptive title (3-7 words) for a conversation."},
        {"role":"user","content":"User: check status\n\nAssistant: ok"}
      ]
    }"#;

    // ── matches() ──────────────────────────────────────────────────────────

    #[test]
    fn matches_openai_chat_with_marker_tools() {
        let c = call(wa::OPENAI_CHAT, Some(HERMES_OAI_CHAT), None);
        assert!(HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn matches_anthropic_with_marker_tools() {
        let c = call(wa::ANTHROPIC, Some(HERMES_ANTHROPIC), None);
        assert!(HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn matches_openai_responses_with_marker_tools() {
        let c = call(wa::OPENAI_RESPONSES, Some(HERMES_RESPONSES), None);
        assert!(HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_one_marker_tool() {
        // Need ≥2 markers; one alone is too generic.
        let body = r#"{
          "messages":[{"role":"user","content":"hi"}],
          "tools":[
            {"type":"function","function":{"name":"terminal"}},
            {"type":"function","function":{"name":"skill_view"}}
          ]
        }"#;
        let c = call(wa::OPENAI_CHAT, Some(body), None);
        assert!(!HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_title_generation_call() {
        // No tools array at all → must not match.
        let c = call(wa::OPENAI_CHAT, Some(HERMES_TITLE_GEN), None);
        assert!(!HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_garbage_body() {
        let c = call(wa::OPENAI_CHAT, Some("not json"), None);
        assert!(!HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_unrelated_openai_chat_traffic() {
        // Generic OpenAI Chat client with non-Hermes tools.
        let body = r#"{
          "messages":[{"role":"user","content":"hi"}],
          "tools":[
            {"type":"function","function":{"name":"calculator"}},
            {"type":"function","function":{"name":"weather"}}
          ]
        }"#;
        let c = call(wa::OPENAI_CHAT, Some(body), None);
        assert!(!HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_unknown_wire_api() {
        let mut c = call(wa::OPENAI_CHAT, Some(HERMES_OAI_CHAT), None);
        c.call.wire_api = "future-wire-api";
        assert!(!HermesProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_collide_with_openclaw_marker_tools() {
        // OpenClaw markers (sessions_spawn, subagents) must not be counted
        // for Hermes — Hermes's marker set is disjoint.
        let body = r#"{
          "messages":[{"role":"user","content":"hi"}],
          "tools":[
            {"type":"function","function":{"name":"sessions_spawn"}},
            {"type":"function","function":{"name":"subagents"}},
            {"type":"function","function":{"name":"sessions_list"}}
          ]
        }"#;
        let c = call(wa::OPENAI_CHAT, Some(body), None);
        assert!(!HermesProfile.matches(&c.ctx()));
    }

    // ── extract_session_id ──────────────────────────────────────────────

    #[test]
    fn extract_session_id_uses_first_tool_id_from_response() {
        let resp = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call_abc","type":"function","function":{"name":"terminal","arguments":"{}"}}]}}]}"#;
        let c = call(wa::OPENAI_CHAT, Some(HERMES_OAI_CHAT), Some(resp));
        let ids = HermesProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, "call_abc");
    }

    #[test]
    fn extract_session_id_text_only_response_falls_back_to_gen_prefix() {
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let c = call(wa::OPENAI_CHAT, Some(HERMES_OAI_CHAT), Some(resp));
        let ids = HermesProfile.extract_session_id(&c.ctx()).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
    }

    // ── is_user_turn_start ──────────────────────────────────────────────

    #[test]
    fn is_user_turn_start_text_user_openai_chat() {
        let c = call(wa::OPENAI_CHAT, Some(HERMES_OAI_CHAT), None);
        assert_eq!(HermesProfile.is_user_turn_start(&c.ctx()), Some(true));
    }

    #[test]
    fn is_user_turn_start_false_when_last_role_is_tool() {
        let body = r#"{
          "messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"terminal","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_a","content":"ok"}
          ],
          "tools":[
            {"type":"function","function":{"name":"skill_view"}},
            {"type":"function","function":{"name":"delegate_task"}}
          ]
        }"#;
        let c = call(wa::OPENAI_CHAT, Some(body), None);
        assert_eq!(HermesProfile.is_user_turn_start(&c.ctx()), Some(false));
    }

    // ── extract_user_input ──────────────────────────────────────────────

    #[test]
    fn extract_user_input_returns_last_user_text() {
        let c = call(wa::OPENAI_CHAT, Some(HERMES_OAI_CHAT), None);
        assert_eq!(
            HermesProfile.extract_user_input(&c.ctx()).as_deref(),
            Some("check status"),
        );
    }

    // ── extract_assistant_text ──────────────────────────────────────────

    #[test]
    fn extract_assistant_text_from_choices_message_content() {
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":"final answer"}}]}"#;
        let c = call(wa::OPENAI_CHAT, None, Some(resp));
        assert_eq!(
            HermesProfile.extract_assistant_text(&c.ctx()).as_deref(),
            Some("final answer"),
        );
    }

    // ── is_turn_terminal ────────────────────────────────────────────────

    #[test]
    fn is_turn_terminal_openai_responses_uses_body_only_check() {
        let wires = crate::wire_apis::build_default_wire_api_registry();
        let resp_terminal = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}"#;
        let mut c = call(wa::OPENAI_RESPONSES, None, Some(resp_terminal));
        assert!(HermesProfile.is_turn_terminal(&c.ctx(), &wires));
        c.set_response_body(
            r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#,
        );
        assert!(!HermesProfile.is_turn_terminal(&c.ctx(), &wires));
    }
}
