//! Concrete AgentProfile implementations — one module per supported agent
//! client. To add a new agent: write a new module here, impl `AgentProfile`,
//! and register it in `build_default_registry()` below.

use crate::profile::AgentProfileRegistry;

pub mod claude_cli;
pub mod codex_cli;
pub mod generic_anthropic;
pub mod generic_common;
pub mod generic_openai_chat;
pub mod generic_openai_responses;

/// Default registry with all built-in agent profiles.
///
/// Order is priority — first match wins. Specific profiles (claude-cli,
/// codex-cli) come first; generic profiles catch traffic without
/// distinguishing client headers.
pub fn build_default_registry() -> AgentProfileRegistry {
    AgentProfileRegistry::new()
        .with(Box::new(claude_cli::ClaudeCliProfile))
        .with(Box::new(codex_cli::CodexCliProfile))
        .with(Box::new(generic_anthropic::GenericAnthropicProfile))
        .with(Box::new(generic_openai_chat::GenericOpenAiChatProfile))
        .with(Box::new(generic_openai_responses::GenericOpenAiResponsesProfile))
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use crate::wire_apis as wa;
    use std::net::IpAddr;

    fn call_with(wire_api: &'static str, headers: Vec<(&str, &str)>) -> LlmCall {
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
            request_body: None,
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
            request_headers: headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            response_headers: vec![],
        }
    }

    #[test]
    fn claude_cli_wins_over_generic_anthropic() {
        let reg = build_default_registry();
        let c = call_with(wa::ANTHROPIC, vec![("User-Agent", "claude-cli/2.1.98 (cli)")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("claude-cli"));
    }

    #[test]
    fn generic_anthropic_catches_no_ua_anthropic() {
        let reg = build_default_registry();
        let c = call_with(wa::ANTHROPIC, vec![("User-Agent", "python/3.12 anthropic/0.40")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic-anthropic"));
    }

    #[test]
    fn codex_cli_wins_over_generic_responses_by_originator() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_RESPONSES, vec![("Originator", "codex_cli_rs")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("codex-cli"));
    }

    #[test]
    fn codex_cli_wins_over_generic_responses_by_ua() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_RESPONSES, vec![("User-Agent", "codex-tui/0.118.0")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("codex-cli"));
    }

    #[test]
    fn generic_responses_catches_no_codex_metadata() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_RESPONSES, vec![("User-Agent", "OpenAI/Python 1.50")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic-openai-responses"));
    }

    #[test]
    fn generic_chat_catches_openai_chat() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_CHAT, vec![("User-Agent", "OpenAI/JS 6.26.0")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic-openai-chat"));
    }
}
