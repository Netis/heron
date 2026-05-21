//! Opencode CLI agent profile.
//!
//! opencode (https://opencode.ai) is a TypeScript/Bun CLI that talks the
//! OpenAI Chat Completions wire shape. Recent releases (≥ 1.14.x as
//! verified against captured pcaps on wuneng) include a stable session
//! identifier on every request:
//!
//!   User-Agent: opencode/1.14.50 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13
//!   x-session-affinity: ses_1d9b5b09affe2vyaYUaMI4M5aS
//!
//! Without a dedicated profile, opencode traffic falls through to
//! GenericProfile, which derives session_id from
//! `text_hash(first_user_text + first_assistant_sig)`. That hash is not
//! stable across the same chat session because the conversation prefix
//! grows between user turns — so a long opencode chat fragments into
//! several `gen-*` session ids and the UI shows each user turn as a
//! detached 1-call AgentTurn. Pulling `x-session-affinity` straight off
//! the wire keeps the whole conversation under one stable session.

use crate::model::LlmCall;
use crate::profile::{AgentProfile, CallCtx, SessionIdExtraction};
use crate::wire_apis as wa;

pub struct OpencodeProfile;

const SESSION_HEADER: &str = "x-session-affinity";
const UA_PREFIX: &str = "opencode/";

fn header<'a>(call: &'a LlmCall, key: &str) -> Option<&'a str> {
    call.request_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

impl AgentProfile for OpencodeProfile {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn matches(&self, ctx: &CallCtx<'_>) -> bool {
        // Mirror ClaudeCliProfile/CodexCliProfile: require BOTH the
        // identifier (UA prefix) AND the session header. UA alone would
        // make us claim pre-1.14.x opencode calls that lack the header,
        // then return None from extract_session_id with no fallback —
        // GenericProfile is a better landing zone for those.
        if ctx.call.wire_api != wa::OPENAI_CHAT {
            return false;
        }
        let ua_ok = header(ctx.call, "user-agent")
            .map(|ua| ua.starts_with(UA_PREFIX))
            .unwrap_or(false);
        if !ua_ok {
            return false;
        }
        header(ctx.call, SESSION_HEADER).is_some()
    }

    fn extract_session_id(&self, ctx: &CallCtx<'_>) -> Option<SessionIdExtraction> {
        let session_id = header(ctx.call, SESSION_HEADER)?.to_string();
        Some(SessionIdExtraction {
            session_id,
            tool_id_canonicalized: false,
        })
    }

    fn extract_user_input(&self, ctx: &CallCtx<'_>) -> Option<String> {
        wa::openai::chat::extract_user_input(ctx.req?)
    }

    fn extract_assistant_text(&self, ctx: &CallCtx<'_>) -> Option<String> {
        wa::openai::chat::extract_assistant_text_value(ctx.resp?)
    }

    fn is_user_turn_start(&self, ctx: &CallCtx<'_>) -> Option<bool> {
        wa::openai::chat::is_user_turn_start(ctx.req?)
    }

    // is_turn_terminal: default (finish_reason="stop" terminal,
    // "tool_calls" not) is correct for opencode — it speaks plain
    // openai-chat and the finish_reason channel is reliable.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use crate::profile::TestCall;
    use std::net::IpAddr;

    fn call_with(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        body: Option<&str>,
    ) -> TestCall {
        TestCall::new(LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "qwen35-27b".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/chat/completions".into(),
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

    const STUB_SESSION: &str = "ses_1d9b5b09affe2vyaYUaMI4M5aS";
    const STUB_UA: &str =
        "opencode/1.14.50 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13";

    #[test]
    fn matches_openai_chat_with_ua_and_session_header() {
        let c = call_with(
            wa::OPENAI_CHAT,
            vec![
                ("User-Agent", STUB_UA),
                ("x-session-affinity", STUB_SESSION),
            ],
            None,
        );
        assert!(OpencodeProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_other_wire_api() {
        let c = call_with(
            wa::ANTHROPIC,
            vec![
                ("User-Agent", STUB_UA),
                ("x-session-affinity", STUB_SESSION),
            ],
            None,
        );
        assert!(!OpencodeProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_other_user_agent() {
        let c = call_with(
            wa::OPENAI_CHAT,
            vec![
                ("User-Agent", "AsyncOpenAI/Python 2.32.0"),
                ("x-session-affinity", STUB_SESSION),
            ],
            None,
        );
        assert!(!OpencodeProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_when_session_header_missing() {
        // Pre-1.14.x opencode versions don't emit x-session-affinity.
        // UA alone is not enough — we'd hand back a None session_id with
        // no fallback. Fall through to GenericProfile instead.
        let c = call_with(
            wa::OPENAI_CHAT,
            vec![("User-Agent", "opencode/1.1.31 ai-sdk/provider-utils/3.0.20 runtime/bun/1.3.5")],
            None,
        );
        assert!(!OpencodeProfile.matches(&c.ctx()));
    }

    #[test]
    fn extract_session_id_returns_x_session_affinity_value() {
        let c = call_with(
            wa::OPENAI_CHAT,
            vec![
                ("User-Agent", STUB_UA),
                ("x-session-affinity", STUB_SESSION),
            ],
            None,
        );
        let ids = OpencodeProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, STUB_SESSION);
        assert!(!ids.tool_id_canonicalized);
    }

    #[test]
    fn extract_session_id_case_insensitive_header_match() {
        // The HTTP capture pipeline may normalize header casing differently
        // across paths. Make sure the lookup tolerates either form.
        let c = call_with(
            wa::OPENAI_CHAT,
            vec![
                ("User-Agent", STUB_UA),
                ("X-Session-Affinity", STUB_SESSION),
            ],
            None,
        );
        let ids = OpencodeProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, STUB_SESSION);
    }

    #[test]
    fn is_user_turn_start_true_when_last_message_is_user_text() {
        let body = r#"{"messages":[
            {"role":"system","content":"sys"},
            {"role":"user","content":"hi"}
        ]}"#;
        let c = call_with(wa::OPENAI_CHAT, vec![], Some(body));
        assert_eq!(OpencodeProfile.is_user_turn_start(&c.ctx()), Some(true));
    }

    #[test]
    fn is_user_turn_start_false_after_tool_response() {
        // role=tool is the openai-chat continuation shape after a tool_calls
        // response. Must not look like a fresh user turn.
        let body = r#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"c","content":"ok"}
        ]}"#;
        let c = call_with(wa::OPENAI_CHAT, vec![], Some(body));
        assert_eq!(OpencodeProfile.is_user_turn_start(&c.ctx()), Some(false));
    }

    #[test]
    fn extract_user_input_returns_last_user_message_text() {
        let body = r#"{"messages":[
            {"role":"user","content":"first"},
            {"role":"assistant","content":"reply"},
            {"role":"user","content":"second"}
        ]}"#;
        let c = call_with(wa::OPENAI_CHAT, vec![], Some(body));
        assert_eq!(
            OpencodeProfile.extract_user_input(&c.ctx()).as_deref(),
            Some("second"),
        );
    }

    #[test]
    fn extract_assistant_text_from_choices_message_content() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"the answer"}}]}"#;
        let mut c = call_with(wa::OPENAI_CHAT, vec![], None);
        c.set_response_body(body);
        assert_eq!(
            OpencodeProfile.extract_assistant_text(&c.ctx()).as_deref(),
            Some("the answer"),
        );
    }

    /// Two calls from the same opencode chat session, captured at
    /// different user turns (different `first_user_text`, growing
    /// conversation prefix). Generic profile would derive divergent
    /// `gen-*` ids; opencode profile must return the SAME session_id
    /// because the wire pins it down via the affinity header.
    ///
    /// This is the regression that motivated the new profile: a long
    /// opencode chat splintered into multiple AgentSessions.
    #[test]
    fn two_calls_with_same_affinity_share_session_id_across_user_turns() {
        let mk = |body: &str| {
            call_with(
                wa::OPENAI_CHAT,
                vec![
                    ("User-Agent", STUB_UA),
                    ("x-session-affinity", STUB_SESSION),
                ],
                Some(body),
            )
        };
        let early = mk(r#"{"messages":[{"role":"user","content":"first question"}]}"#);
        let later = mk(r#"{"messages":[
            {"role":"user","content":"first question"},
            {"role":"assistant","content":"first reply"},
            {"role":"user","content":"fifth question — much later"}
        ]}"#);
        let id_early = OpencodeProfile
            .extract_session_id(&early.ctx())
            .unwrap()
            .session_id;
        let id_later = OpencodeProfile
            .extract_session_id(&later.ctx())
            .unwrap()
            .session_id;
        assert_eq!(id_early, id_later, "affinity header must pin session id");
        assert_eq!(id_early, STUB_SESSION);
    }
}
