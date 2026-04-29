use crate::model::LlmCall;
use crate::wire_api_registry::WireApiRegistry;

/// Per-agent knowledge about how to extract a session id and identify
/// whether a call is user-initiated. Each concrete impl represents one
/// agent client (e.g. `claude-cli`, `codex-cli`).
pub trait AgentProfile: Send + Sync {
    /// Short stable name (e.g. `"claude-cli"`). Persisted to storage as
    /// `AgentTurn.agent_kind`.
    fn name(&self) -> &'static str;

    /// Return true iff this profile handles the given call.
    /// Implementations typically check `wire_api` + User-Agent / Originator header.
    fn matches(&self, call: &LlmCall) -> bool;

    /// Extract the session id this call belongs to, plus any derivation
    /// metadata (see `SessionIdExtraction`). Returning `None` means matching
    /// failed at a deeper level (e.g., header missing); the call will be
    /// flagged as unassociated and skipped by the tracker.
    fn extract_session_id(&self, call: &LlmCall) -> Option<SessionIdExtraction>;

    /// Decide whether this call's *body* represents a fresh user-initiated
    /// message. `Some(true)` = body is a fresh user prompt; `Some(false)` =
    /// tool-result / continuation body; `None` = cannot decide (body missing
    /// or unparseable).
    ///
    /// **Raw structural answer.** Sub-agent / auxiliary filtering is applied
    /// by the ts-llm stage when it builds `AgentCallInfo`, not inside the
    /// profile — a sub-agent dispatch with fresh user text correctly returns
    /// `Some(true)` here, and the consumer combines it with `subagent_name`
    /// to decide whether a *main-agent* turn starts.
    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool>;

    /// Extract the sub-agent tag (e.g., Codex "review"). `None` = main agent.
    fn subagent(&self, call: &LlmCall) -> Option<String> {
        let _ = call;
        None
    }

    /// Return true if this call is an auxiliary one-shot request that should
    /// not participate in turn tracking at all (e.g., claude-cli's session
    /// title generation). Tracker drops such calls entirely.
    fn is_auxiliary(&self, call: &LlmCall) -> bool {
        let _ = call;
        false
    }

    /// Extract the user-visible prompt that initiated a turn. Called on turn
    /// creation. Returns the concatenated user text with internal scaffolding
    /// (e.g., Claude Code `<system-reminder>` blocks) stripped. `None` when
    /// body is absent or unparseable.
    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let _ = call;
        None
    }

    /// Extract the final assistant text from a call's response body. Called
    /// when the tracker closes a turn on a terminal finish_reason, using that
    /// last call's response. `None` when body is absent or empty.
    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let _ = call;
        None
    }

    /// Decide whether this call is the agent-turn terminator — i.e., the
    /// model has produced a final answer and no further API call is expected
    /// in this turn.
    ///
    /// **Default** runs the implicit-path dispatch via the wire API: a call
    /// is terminal iff its `finish_reason` is wire-terminal AND not
    /// `tool_use`. This covers Anthropic, OpenAI Chat, and any other profile
    /// whose turn boundary is faithfully encoded in `finish_reason`.
    ///
    /// **Override** when the wire-api `finish_reason` cannot distinguish
    /// "agent done" from "tool roundtrip pending" (e.g. Codex over
    /// openai-responses, where every successful API call reports
    /// `response.completed`). Overrides own the full decision and do NOT
    /// fall through to the default — if a profile inspects the response
    /// body explicitly, the wire-api signal is presumed unreliable.
    fn is_turn_terminal(&self, call: &LlmCall, wire_apis: &WireApiRegistry) -> bool {
        let Some(reason) = call.finish_reason.as_deref() else {
            return false;
        };
        let Some(api) = wire_apis.find_by_name(call.wire_api) else {
            return false;
        };
        api.is_terminal(reason) && !api.is_tool_use(reason)
    }
}

/// Output of `AgentProfile::extract_session_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdExtraction {
    pub session_id: String,
    /// True if the tool id used to derive `session_id` was modified by
    /// `canonicalize_tool_id`. Used by the llm stage to bump
    /// `LlmGenericToolIdCanonicalized`.
    pub tool_id_canonicalized: bool,
}

/// First-match registry. Order matters: the first matching profile wins.
pub struct AgentProfileRegistry {
    profiles: Vec<Box<dyn AgentProfile>>,
}

impl AgentProfileRegistry {
    pub fn new() -> Self {
        Self {
            profiles: Vec::new(),
        }
    }

    pub fn with(mut self, profile: Box<dyn AgentProfile>) -> Self {
        self.profiles.push(profile);
        self
    }

    pub fn find(&self, call: &LlmCall) -> Option<&dyn AgentProfile> {
        self.profiles
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.matches(call))
    }

    pub fn find_by_name(&self, name: &str) -> Option<&dyn AgentProfile> {
        self.profiles
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.name() == name)
    }
}

impl Default for AgentProfileRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use crate::wire_apis as wa;
    use std::net::IpAddr;

    fn stub_call(ua: &str) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api: wa::ANTHROPIC,
            model: "m".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/messages".into(),
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
            request_headers: vec![("User-Agent".into(), ua.into())],
            response_headers: vec![],
        }
    }

    struct FakeProfile {
        ua_prefix: &'static str,
        name: &'static str,
    }
    impl AgentProfile for FakeProfile {
        fn name(&self) -> &'static str {
            self.name
        }
        fn matches(&self, call: &LlmCall) -> bool {
            call.request_headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("user-agent") && v.starts_with(self.ua_prefix))
        }
        fn extract_session_id(&self, _: &LlmCall) -> Option<SessionIdExtraction> {
            Some(SessionIdExtraction {
                session_id: "s".into(),
                tool_id_canonicalized: false,
            })
        }
        fn is_user_turn_start(&self, _: &LlmCall) -> Option<bool> {
            None
        }
    }

    #[test]
    fn registry_first_match_wins() {
        let reg = AgentProfileRegistry::new()
            .with(Box::new(FakeProfile {
                ua_prefix: "alpha/",
                name: "alpha",
            }))
            .with(Box::new(FakeProfile {
                ua_prefix: "beta/",
                name: "beta",
            }));
        assert_eq!(reg.find(&stub_call("alpha/1.0")).unwrap().name(), "alpha");
        assert_eq!(reg.find(&stub_call("beta/2.0")).unwrap().name(), "beta");
        assert!(reg.find(&stub_call("gamma/3.0")).is_none());
    }

    #[test]
    fn find_by_name_returns_matching_profile() {
        let reg = AgentProfileRegistry::new()
            .with(Box::new(FakeProfile {
                ua_prefix: "alpha/",
                name: "alpha",
            }))
            .with(Box::new(FakeProfile {
                ua_prefix: "beta/",
                name: "beta",
            }));
        assert_eq!(reg.find_by_name("alpha").map(|p| p.name()), Some("alpha"));
        assert_eq!(reg.find_by_name("beta").map(|p| p.name()), Some("beta"));
        assert!(reg.find_by_name("gamma").is_none());
    }
}
