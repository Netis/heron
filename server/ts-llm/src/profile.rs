use crate::model::LlmCall;
use crate::wire_api_registry::WireApiRegistry;
use serde_json::Value;

/// Per-call context handed to every `AgentProfile` method. Bodies are
/// pre-parsed once at the boundary (`build_agent_call_info` and the
/// off-hot-path extractors in `ts-storage`); methods read `ctx.req` /
/// `ctx.resp` directly instead of running `serde_json::from_str`
/// internally. `None` means the body was absent or non-JSON — handled the
/// same way the per-method `serde_json::from_str(body).ok()?` pattern did
/// before this refactor.
pub struct CallCtx<'a> {
    pub call: &'a LlmCall,
    pub req: Option<&'a Value>,
    pub resp: Option<&'a Value>,
}

impl<'a> CallCtx<'a> {
    pub fn new(call: &'a LlmCall, req: Option<&'a Value>, resp: Option<&'a Value>) -> Self {
        Self { call, req, resp }
    }
}

/// Parse both bodies of an `LlmCall` once. Caller holds the resulting
/// `Option<Value>`s on the stack and borrows them into a `CallCtx`. Used at
/// every boundary into `AgentProfile`.
pub fn parse_bodies(call: &LlmCall) -> (Option<Value>, Option<Value>) {
    let req = call
        .request_body
        .as_deref()
        .and_then(|b| serde_json::from_str(b).ok());
    let resp = call
        .response_body
        .as_deref()
        .and_then(|b| serde_json::from_str(b).ok());
    (req, resp)
}

/// Test-only wrapper that owns an `LlmCall` and its pre-parsed bodies, so
/// tests can construct one and call `tc.ctx()` to get a borrowed `CallCtx`.
/// Used by every agent test module to avoid scattering 3-line parse
/// boilerplate across ~130 tests.
#[cfg(test)]
pub(crate) struct TestCall {
    pub call: LlmCall,
    pub req: Option<Value>,
    pub resp: Option<Value>,
}

#[cfg(test)]
impl TestCall {
    pub fn new(call: LlmCall) -> Self {
        let (req, resp) = parse_bodies(&call);
        Self { call, req, resp }
    }
    pub fn ctx(&self) -> CallCtx<'_> {
        CallCtx::new(&self.call, self.req.as_ref(), self.resp.as_ref())
    }
    pub fn set_response_body(&mut self, body: impl Into<String>) {
        self.call.response_body = Some(body.into());
        let (req, resp) = parse_bodies(&self.call);
        self.req = req;
        self.resp = resp;
    }
}

/// Per-agent knowledge about how to extract a session id and identify
/// whether a call is user-initiated. Each concrete impl represents one
/// agent client (e.g. `claude-cli`, `codex-cli`).
pub trait AgentProfile: Send + Sync {
    /// Short stable name (e.g. `"claude-cli"`). Persisted to storage as
    /// `AgentTurn.agent_kind`.
    fn name(&self) -> &'static str;

    /// Return true iff this profile handles the given call.
    /// Implementations typically check `wire_api` + User-Agent / Originator header.
    fn matches(&self, ctx: &CallCtx<'_>) -> bool;

    /// Extract the session id this call belongs to, plus any derivation
    /// metadata (see `SessionIdExtraction`). Returning `None` means matching
    /// failed at a deeper level (e.g., header missing); the call will be
    /// flagged as unassociated and skipped by the tracker.
    fn extract_session_id(&self, ctx: &CallCtx<'_>) -> Option<SessionIdExtraction>;

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
    fn is_user_turn_start(&self, ctx: &CallCtx<'_>) -> Option<bool>;

    /// Extract the sub-agent tag (e.g., Codex "review"). `None` = main agent.
    fn subagent(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let _ = ctx;
        None
    }

    /// Return true if this call is an auxiliary one-shot request that should
    /// not participate in turn tracking at all (e.g., claude-cli's session
    /// title generation). Tracker drops such calls entirely.
    fn is_auxiliary(&self, ctx: &CallCtx<'_>) -> bool {
        let _ = ctx;
        false
    }

    /// Extract the user-visible prompt that initiated a turn. Called on turn
    /// creation. Returns the concatenated user text with internal scaffolding
    /// (e.g., Claude Code `<system-reminder>` blocks) stripped. `None` when
    /// body is absent or unparseable.
    fn extract_user_input(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let _ = ctx;
        None
    }

    /// Extract the final assistant text from a call's response body. Called
    /// when the tracker closes a turn on a terminal finish_reason, using that
    /// last call's response. `None` when body is absent or empty.
    fn extract_assistant_text(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let _ = ctx;
        None
    }

    /// Extract primitive facts about this call for the agent classifier.
    /// Default implementation returns inert primitives — profiles that
    /// understand their wire shape override this to fill in tool counts,
    /// names, system-prompt markers, and sub-agent dispatch info.
    fn extract_primitives(&self, _ctx: &CallCtx<'_>) -> crate::agent_primitives::AgentPrimitives {
        crate::agent_primitives::AgentPrimitives::default()
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
    fn is_turn_terminal(&self, ctx: &CallCtx<'_>, wire_apis: &WireApiRegistry) -> bool {
        let Some(reason) = ctx.call.finish_reason.as_deref() else {
            return false;
        };
        let Some(api) = wire_apis.find_by_name(ctx.call.wire_api) else {
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

    pub fn find(&self, ctx: &CallCtx<'_>) -> Option<&dyn AgentProfile> {
        self.profiles
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.matches(ctx))
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
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
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
        fn matches(&self, ctx: &CallCtx<'_>) -> bool {
            ctx.call
                .request_headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("user-agent") && v.starts_with(self.ua_prefix))
        }
        fn extract_session_id(&self, _: &CallCtx<'_>) -> Option<SessionIdExtraction> {
            Some(SessionIdExtraction {
                session_id: "s".into(),
                tool_id_canonicalized: false,
            })
        }
        fn is_user_turn_start(&self, _: &CallCtx<'_>) -> Option<bool> {
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
        let alpha = stub_call("alpha/1.0");
        let beta = stub_call("beta/2.0");
        let gamma = stub_call("gamma/3.0");
        assert_eq!(
            reg.find(&CallCtx::new(&alpha, None, None)).unwrap().name(),
            "alpha"
        );
        assert_eq!(
            reg.find(&CallCtx::new(&beta, None, None)).unwrap().name(),
            "beta"
        );
        assert!(reg.find(&CallCtx::new(&gamma, None, None)).is_none());
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
