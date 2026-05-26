//! Concrete AgentProfile implementations — one module per supported agent
//! client. To add a new agent: write a new module here, impl `AgentProfile`,
//! and register it in `build_default_registry()` below.

use crate::profile::AgentProfileRegistry;

pub mod claude_cli;
pub mod codex_cli;
pub mod generic;
pub mod hermes;
pub mod openclaw;
pub mod opencode;
pub mod session_id;

pub use claude_cli::ClaudeCliProfile;
pub use codex_cli::CodexCliProfile;
pub use generic::GenericProfile;
pub use hermes::HermesProfile;
pub use openclaw::OpenClawProfile;
pub use opencode::OpencodeProfile;

/// Default registry with all built-in agent profiles.
///
/// Order is priority — first match wins. Specific profiles (claude-cli,
/// codex-cli, opencode, openclaw, hermes) come first; the generic profile
/// catches traffic without distinguishing client headers, dispatching
/// internally on `wire_api`. Header-based detection (claude-cli, codex-cli,
/// opencode) precedes body-fingerprint detection (openclaw, hermes) so
/// cheaper checks run first. OpenClaw and Hermes both fingerprint by
/// `tools[]` marker names but their marker sets are disjoint — order
/// between them is irrelevant.
pub fn build_default_registry() -> AgentProfileRegistry {
    AgentProfileRegistry::new()
        .with(Box::new(claude_cli::ClaudeCliProfile))
        .with(Box::new(codex_cli::CodexCliProfile))
        .with(Box::new(opencode::OpencodeProfile))
        .with(Box::new(openclaw::OpenClawProfile))
        .with(Box::new(hermes::HermesProfile))
        .with(Box::new(generic::GenericProfile))
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use crate::profile::{parse_bodies, CallCtx};
    use crate::wire_apis as wa;
    use std::net::IpAddr;

    fn find_kind(reg: &AgentProfileRegistry, c: &LlmCall) -> Option<&'static str> {
        let (req, resp) = parse_bodies(c);
        let ctx = CallCtx::new(c, req.as_ref(), resp.as_ref());
        reg.find(&ctx).map(|p| p.name())
    }

    fn call_with(wire_api: &'static str, headers: Vec<(&str, &str)>) -> LlmCall {
        call_with_body(wire_api, headers, None)
    }

    fn call_with_body(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        body: Option<&str>,
    ) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "m".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/".into(),
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
        }
    }

    /// Anthropic main-path body (≥2 OpenClaw marker tools).
    const OPENCLAW_ANT_MAIN: &str = r#"{
      "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
      "tools":[
        {"name":"read"},{"name":"write"},
        {"name":"sessions_spawn"},{"name":"subagents"}
      ]
    }"#;

    /// Anthropic summarizer-aux body (empty tools + summarizer system).
    const OPENCLAW_ANT_AUX: &str = r#"{
      "system":[{"type":"text","text":"You are a context summarization assistant."}],
      "messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],
      "tools":[]
    }"#;

    /// OpenAI Chat main-path body (≥2 OpenClaw marker tools).
    const OPENCLAW_OAI_MAIN: &str = r#"{
      "messages":[
        {"role":"system","content":"You are a personal assistant running inside OpenClaw."},
        {"role":"user","content":"hi"}
      ],
      "tools":[
        {"type":"function","function":{"name":"read"}},
        {"type":"function","function":{"name":"sessions_spawn"}},
        {"type":"function","function":{"name":"subagents"}}
      ]
    }"#;

    /// OpenAI Responses main-path body (flat tools, ≥2 OpenClaw markers).
    const OPENCLAW_RESPONSES_MAIN: &str = r#"{
      "input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
      ],
      "tools":[
        {"type":"function","name":"read"},
        {"type":"function","name":"sessions_spawn"},
        {"type":"function","name":"subagents"}
      ]
    }"#;

    const GENERIC_ANT_TOOL_HISTORY: &str = r#"{
      "messages":[
        {"role":"user","content":[{"type":"text","text":"hi"}]},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_generic","name":"Read","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_generic","content":"ok"}]}
      ]
    }"#;

    const GENERIC_OAI_CHAT_TOOL_HISTORY: &str = r#"{
      "messages":[
        {"role":"user","content":"hi"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_generic","type":"function","function":{"name":"f","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call_generic","content":"ok"}
      ]
    }"#;

    const GENERIC_RESPONSES_TOOL_HISTORY: &str = r#"{
      "input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
        {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_generic"},
        {"type":"function_call_output","call_id":"fc_generic","output":"ok"}
      ]
    }"#;

    #[test]
    fn claude_cli_wins_over_generic_for_anthropic() {
        let reg = build_default_registry();
        let c = call_with(
            wa::ANTHROPIC,
            vec![
                ("User-Agent", "claude-cli/2.1.98 (cli)"),
                (
                    "X-Claude-Code-Session-Id",
                    "deadbeef-0000-0000-0000-000000000000",
                ),
            ],
        );
        assert_eq!(find_kind(&reg, &c), Some("claude-cli"));
    }

    #[test]
    fn generic_catches_no_ua_anthropic() {
        let reg = build_default_registry();
        let c = call_with_body(
            wa::ANTHROPIC,
            vec![("User-Agent", "python/3.12 anthropic/0.40")],
            Some(GENERIC_ANT_TOOL_HISTORY),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    #[test]
    fn generic_catches_claude_cli_ua_without_session_header() {
        // UA-spoofing or header-stripping: looks like claude-cli but the
        // session header is absent. ClaudeCliProfile must NOT claim it
        // (otherwise extract_session_id would return None with no fallback) —
        // GenericProfile picks it up when the body has a tool-call anchor.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::ANTHROPIC,
            vec![("User-Agent", "claude-cli/2.1.98 (cli)")],
            Some(GENERIC_ANT_TOOL_HISTORY),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    const CODEX_STUB_META: &str = r#"{"session_id":"deadbeef-0000-0000-0000-000000000000"}"#;

    #[test]
    fn codex_cli_wins_over_generic_by_originator() {
        let reg = build_default_registry();
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![
                ("Originator", "codex_cli_rs"),
                ("X-Codex-Turn-Metadata", CODEX_STUB_META),
            ],
        );
        assert_eq!(find_kind(&reg, &c), Some("codex-cli"));
    }

    #[test]
    fn codex_cli_wins_over_generic_by_ua() {
        let reg = build_default_registry();
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![
                ("User-Agent", "codex-tui/0.118.0"),
                ("X-Codex-Turn-Metadata", CODEX_STUB_META),
            ],
        );
        assert_eq!(find_kind(&reg, &c), Some("codex-cli"));
    }

    #[test]
    fn generic_catches_no_codex_metadata_responses() {
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "OpenAI/Python 1.50")],
            Some(GENERIC_RESPONSES_TOOL_HISTORY),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    #[test]
    fn generic_catches_codex_ua_without_turn_metadata_header() {
        // UA-spoofing or header-stripping: looks like codex but the
        // turn-metadata header is absent. CodexCliProfile must NOT claim it
        // (otherwise extract_session_id would return None with no fallback) —
        // GenericProfile picks it up when the body has a tool-call anchor.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "codex-tui/0.118.0")],
            Some(GENERIC_RESPONSES_TOOL_HISTORY),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    #[test]
    fn generic_catches_openai_chat() {
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_CHAT,
            vec![("User-Agent", "OpenAI/JS 6.26.0")],
            Some(GENERIC_OAI_CHAT_TOOL_HISTORY),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    #[test]
    fn opencode_wins_over_generic_when_ua_and_session_header_present() {
        let reg = build_default_registry();
        let c = call_with(
            wa::OPENAI_CHAT,
            vec![
                (
                    "User-Agent",
                    "opencode/1.14.50 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13",
                ),
                ("x-session-affinity", "ses_1d9b5b09affe2vyaYUaMI4M5aS"),
            ],
        );
        assert_eq!(find_kind(&reg, &c), Some("opencode"));
    }

    #[test]
    fn generic_catches_old_opencode_without_session_header() {
        // Pre-1.14.x opencode lacks x-session-affinity. OpencodeProfile must
        // NOT claim it (no fallback for session_id), so the call falls
        // through to GenericProfile when the body has a tool-call anchor.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_CHAT,
            vec![(
                "User-Agent",
                "opencode/1.1.31 ai-sdk/provider-utils/3.0.20 runtime/bun/1.3.5",
            )],
            Some(GENERIC_OAI_CHAT_TOOL_HISTORY),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    #[test]
    fn openclaw_wins_over_generic_when_anthropic_marker_tools() {
        let reg = build_default_registry();
        let c = call_with_body(wa::ANTHROPIC, vec![], Some(OPENCLAW_ANT_MAIN));
        assert_eq!(find_kind(&reg, &c), Some("openclaw"));
    }

    #[test]
    fn openclaw_wins_over_generic_when_openai_chat_marker_tools() {
        let reg = build_default_registry();
        let c = call_with_body(wa::OPENAI_CHAT, vec![], Some(OPENCLAW_OAI_MAIN));
        assert_eq!(find_kind(&reg, &c), Some("openclaw"));
    }

    #[test]
    fn openclaw_wins_over_generic_when_openai_responses_marker_tools() {
        let reg = build_default_registry();
        let c = call_with_body(wa::OPENAI_RESPONSES, vec![], Some(OPENCLAW_RESPONSES_MAIN));
        assert_eq!(find_kind(&reg, &c), Some("openclaw"));
    }

    #[test]
    fn codex_cli_still_wins_over_openclaw_on_responses_when_codex_header_present() {
        // codex-cli is registered before openclaw — even if a body somehow
        // carried OpenClaw marker tools, the codex-tui UA wins. Verifies
        // registry-order invariant when extending openclaw to Responses.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_RESPONSES,
            vec![
                ("User-Agent", "codex-tui/0.118.0"),
                ("X-Codex-Turn-Metadata", CODEX_STUB_META),
            ],
            Some(OPENCLAW_RESPONSES_MAIN),
        );
        assert_eq!(find_kind(&reg, &c), Some("codex-cli"));
    }

    #[test]
    fn openclaw_wins_for_summarizer_aux_anthropic() {
        let reg = build_default_registry();
        let c = call_with_body(wa::ANTHROPIC, vec![], Some(OPENCLAW_ANT_AUX));
        assert_eq!(find_kind(&reg, &c), Some("openclaw"));
    }

    #[test]
    fn generic_still_wins_for_non_openclaw_anthropic() {
        // Body lacks OpenClaw marker tools but has a tool-call anchor →
        // falls through to generic.
        let reg = build_default_registry();
        let body = r#"{
          "messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"toolu_generic","name":"Read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_generic","content":"ok"}]}
          ],
          "tools":[{"name":"Read"},{"name":"Edit"}]
        }"#;
        let c = call_with_body(
            wa::ANTHROPIC,
            vec![("User-Agent", "python/3.12 anthropic/0.40")],
            Some(body),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    #[test]
    fn generic_still_wins_for_non_openclaw_openai_chat() {
        let reg = build_default_registry();
        let body = r#"{
          "messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_generic","type":"function","function":{"name":"calculator","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_generic","content":"ok"}
          ],
          "tools":[{"type":"function","function":{"name":"calculator"}}]
        }"#;
        let c = call_with_body(
            wa::OPENAI_CHAT,
            vec![("User-Agent", "OpenAI/JS 6.26.0")],
            Some(body),
        );
        assert_eq!(find_kind(&reg, &c), Some("generic"));
    }

    /// Hermes OpenAI Chat body — ≥2 Hermes marker tools.
    const HERMES_OAI_CHAT: &str = r##"{
      "messages":[
        {"role":"system","content":"# Hermes Agent Persona"},
        {"role":"user","content":"hi"}
      ],
      "tools":[
        {"type":"function","function":{"name":"terminal"}},
        {"type":"function","function":{"name":"skill_view"}},
        {"type":"function","function":{"name":"delegate_task"}}
      ]
    }"##;

    /// Hermes title-generation body — no tools, generic title prompt.
    const HERMES_TITLE_GEN: &str = r#"{
      "messages":[
        {"role":"system","content":"Generate a short, descriptive title (3-7 words) for a conversation."},
        {"role":"user","content":"User: hi\n\nAssistant: hello"}
      ]
    }"#;

    #[test]
    fn hermes_wins_over_generic_when_openai_chat_marker_tools() {
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_CHAT,
            vec![("User-Agent", "OpenAI/Python 2.33.0")],
            Some(HERMES_OAI_CHAT),
        );
        assert_eq!(find_kind(&reg, &c), Some("hermes"));
    }

    #[test]
    fn hermes_title_gen_has_no_agent_profile() {
        // Hermes's chat-title-generation call carries no tools and no
        // Hermes markers, so HermesProfile must NOT match it. Generic
        // fallback also ignores text-only calls, so it stays call-only.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_CHAT,
            vec![("User-Agent", "OpenAI/Python 2.33.0")],
            Some(HERMES_TITLE_GEN),
        );
        assert_eq!(find_kind(&reg, &c), None);
    }

    #[test]
    fn hermes_does_not_collide_with_openclaw_markers() {
        // OpenClaw marker tools (sessions_spawn, subagents) are disjoint
        // from Hermes's marker set. An OpenClaw-shaped body must classify
        // as openclaw, not hermes, regardless of registration order.
        let reg = build_default_registry();
        let c = call_with_body(wa::OPENAI_CHAT, vec![], Some(OPENCLAW_OAI_MAIN));
        assert_eq!(find_kind(&reg, &c), Some("openclaw"));
    }

    #[test]
    fn claude_cli_still_wins_over_openclaw_when_ua_present() {
        // Even if the body somehow contained OpenClaw markers, claude-cli
        // (header-based) is registered earlier and wins. Verifies registry
        // order: header-based before body-fingerprint.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::ANTHROPIC,
            vec![
                ("User-Agent", "claude-cli/2.1.98"),
                (
                    "X-Claude-Code-Session-Id",
                    "deadbeef-0000-0000-0000-000000000000",
                ),
            ],
            Some(OPENCLAW_ANT_MAIN),
        );
        assert_eq!(find_kind(&reg, &c), Some("claude-cli"));
    }
}
