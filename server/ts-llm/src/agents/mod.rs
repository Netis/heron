//! Concrete AgentProfile implementations — one module per supported agent
//! client. To add a new agent: write a new module here, impl `AgentProfile`,
//! and register it in `build_default_registry()` below.

use crate::profile::AgentProfileRegistry;

pub mod claude_cli;
pub mod codex_cli;
pub mod generic;
pub mod openclaw;
pub mod session_id;

/// Default registry with all built-in agent profiles.
///
/// Order is priority — first match wins. Specific profiles (claude-cli,
/// codex-cli, openclaw) come first; the generic profile catches traffic
/// without distinguishing client headers, dispatching internally on
/// `wire_api`. Header-based detection (claude-cli, codex-cli) precedes
/// body-fingerprint detection (openclaw) so cheaper checks run first.
pub fn build_default_registry() -> AgentProfileRegistry {
    AgentProfileRegistry::new()
        .with(Box::new(claude_cli::ClaudeCliProfile))
        .with(Box::new(codex_cli::CodexCliProfile))
        .with(Box::new(openclaw::OpenClawProfile))
        .with(Box::new(generic::GenericProfile))
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use crate::wire_apis as wa;
    use std::net::IpAddr;

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

    #[test]
    fn claude_cli_wins_over_generic_for_anthropic() {
        let reg = build_default_registry();
        let c = call_with(
            wa::ANTHROPIC,
            vec![("User-Agent", "claude-cli/2.1.98 (cli)")],
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("claude-cli"));
    }

    #[test]
    fn generic_catches_no_ua_anthropic() {
        let reg = build_default_registry();
        let c = call_with(
            wa::ANTHROPIC,
            vec![("User-Agent", "python/3.12 anthropic/0.40")],
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic"));
    }

    #[test]
    fn codex_cli_wins_over_generic_by_originator() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_RESPONSES, vec![("Originator", "codex_cli_rs")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("codex-cli"));
    }

    #[test]
    fn codex_cli_wins_over_generic_by_ua() {
        let reg = build_default_registry();
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "codex-tui/0.118.0")],
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("codex-cli"));
    }

    #[test]
    fn generic_catches_no_codex_metadata_responses() {
        let reg = build_default_registry();
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "OpenAI/Python 1.50")],
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic"));
    }

    #[test]
    fn generic_catches_openai_chat() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_CHAT, vec![("User-Agent", "OpenAI/JS 6.26.0")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic"));
    }

    #[test]
    fn openclaw_wins_over_generic_when_anthropic_marker_tools() {
        let reg = build_default_registry();
        let c = call_with_body(wa::ANTHROPIC, vec![], Some(OPENCLAW_ANT_MAIN));
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("openclaw"));
    }

    #[test]
    fn openclaw_wins_over_generic_when_openai_chat_marker_tools() {
        let reg = build_default_registry();
        let c = call_with_body(wa::OPENAI_CHAT, vec![], Some(OPENCLAW_OAI_MAIN));
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("openclaw"));
    }

    #[test]
    fn openclaw_wins_over_generic_when_openai_responses_marker_tools() {
        let reg = build_default_registry();
        let c = call_with_body(wa::OPENAI_RESPONSES, vec![], Some(OPENCLAW_RESPONSES_MAIN));
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("openclaw"));
    }

    #[test]
    fn codex_cli_still_wins_over_openclaw_on_responses_when_codex_header_present() {
        // codex-cli is registered before openclaw — even if a body somehow
        // carried OpenClaw marker tools, the codex-tui UA wins. Verifies
        // registry-order invariant when extending openclaw to Responses.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "codex-tui/0.118.0")],
            Some(OPENCLAW_RESPONSES_MAIN),
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("codex-cli"));
    }

    #[test]
    fn openclaw_wins_for_summarizer_aux_anthropic() {
        let reg = build_default_registry();
        let c = call_with_body(wa::ANTHROPIC, vec![], Some(OPENCLAW_ANT_AUX));
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("openclaw"));
    }

    #[test]
    fn generic_still_wins_for_non_openclaw_anthropic() {
        // Body lacks OpenClaw marker tools → falls through to generic.
        let reg = build_default_registry();
        let body = r#"{
          "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
          "tools":[{"name":"Read"},{"name":"Edit"}]
        }"#;
        let c = call_with_body(
            wa::ANTHROPIC,
            vec![("User-Agent", "python/3.12 anthropic/0.40")],
            Some(body),
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic"));
    }

    #[test]
    fn generic_still_wins_for_non_openclaw_openai_chat() {
        let reg = build_default_registry();
        let body = r#"{
          "messages":[{"role":"user","content":"hi"}],
          "tools":[{"type":"function","function":{"name":"calculator"}}]
        }"#;
        let c = call_with_body(
            wa::OPENAI_CHAT,
            vec![("User-Agent", "OpenAI/JS 6.26.0")],
            Some(body),
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic"));
    }

    #[test]
    fn claude_cli_still_wins_over_openclaw_when_ua_present() {
        // Even if the body somehow contained OpenClaw markers, claude-cli
        // (header-based) is registered earlier and wins. Verifies registry
        // order: header-based before body-fingerprint.
        let reg = build_default_registry();
        let c = call_with_body(
            wa::ANTHROPIC,
            vec![("User-Agent", "claude-cli/2.1.98")],
            Some(OPENCLAW_ANT_MAIN),
        );
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("claude-cli"));
    }
}
