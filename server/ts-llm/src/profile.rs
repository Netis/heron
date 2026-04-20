use crate::model::LlmCall;

/// Per-client knowledge about how to extract session/turn IDs and identify
/// whether a call is user-initiated.
pub trait ClientProfile: Send + Sync {
    /// Short stable name, used as `LlmCall.client_kind`.
    fn name(&self) -> &'static str;

    /// Return true iff this profile handles the given call.
    /// Implementations typically check `wire_api` + User-Agent / Originator header.
    fn matches(&self, call: &LlmCall) -> bool;

    /// Extract the (session_id, optional turn_id) pair.
    /// Returning `None` means matching failed at a deeper level (e.g., header missing);
    /// the call will be flagged as unassociated and skipped by the tracker.
    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds>;

    /// Decide whether this call represents a fresh user-initiated turn start.
    /// `Some(true)` = new turn starts here; `Some(false)` = tool-result continuation;
    /// `None` = cannot decide (e.g., body missing or unparseable) — tracker falls back to
    /// "same turn as last call" behavior.
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
    /// in this turn. Used by the tracker's explicit-turn_id path (Codex) to
    /// close turns immediately instead of waiting for the next turn_id or the
    /// idle-timeout sweep.
    ///
    /// Default `false` preserves the existing implicit-path semantics
    /// (Anthropic), where finish_reason mapping already drives termination.
    fn is_turn_terminal(&self, call: &LlmCall) -> bool {
        let _ = call;
        false
    }
}

/// Output of `ClientProfile::extract_ids`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedIds {
    pub session_id: String,
    /// `None` ⇒ tracker will generate a turn_id via state machine (Anthropic path).
    /// `Some(_)` ⇒ direct grouping (Codex path).
    pub turn_id: Option<String>,
}

/// First-match registry. Order matters: the first matching profile wins.
pub struct ProfileRegistry {
    profiles: Vec<Box<dyn ClientProfile>>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        Self {
            profiles: Vec::new(),
        }
    }

    pub fn with(mut self, profile: Box<dyn ClientProfile>) -> Self {
        self.profiles.push(profile);
        self
    }

    pub fn find(&self, call: &LlmCall) -> Option<&dyn ClientProfile> {
        self.profiles
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.matches(call))
    }

    pub fn find_by_name(&self, name: &str) -> Option<&dyn ClientProfile> {
        self.profiles
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.name() == name)
    }
}

impl Default for ProfileRegistry {
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
            stream_id: String::new(),
            id: "c".into(),
            wire_api: wa::ANTHROPIC_MESSAGES,
            model: "m".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
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
            ttfb_ms: None,
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
    impl ClientProfile for FakeProfile {
        fn name(&self) -> &'static str {
            self.name
        }
        fn matches(&self, call: &LlmCall) -> bool {
            call.request_headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("user-agent") && v.starts_with(self.ua_prefix))
        }
        fn extract_ids(&self, _: &LlmCall) -> Option<ExtractedIds> {
            Some(ExtractedIds {
                session_id: "s".into(),
                turn_id: None,
            })
        }
        fn is_user_turn_start(&self, _: &LlmCall) -> Option<bool> {
            None
        }
    }

    #[test]
    fn registry_first_match_wins() {
        let reg = ProfileRegistry::new()
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
        let reg = ProfileRegistry::new()
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
