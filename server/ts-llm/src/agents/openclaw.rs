//! OpenClaw profile — handles the OpenClaw client across all three of its
//! supported transports: Anthropic `/v1/messages`, OpenAI Chat Completions
//! `/v1/chat/completions`, and OpenAI Responses `/v1/responses`. A single
//! profile with internal `wire_api` dispatch (rather than three per-wire-api
//! profiles) keeps `agent_kind` reflecting the client identity instead of
//! the transport. (Confirmed against `openclaw/src/agents/{openai,
//! anthropic}-transport-stream.ts` — the stream functions branch on
//! `client.responses.create` / `client.chat.completions.create` /
//! `client.messages.stream`, all three reachable from the OpenClaw runtime
//! depending on the configured model.)
//!
//! Identification (header-based detection isn't possible — OpenClaw uses
//! the SDK-default `Anthropic/JS` / `OpenAI/JS` user agents and a generic
//! `X-Stainless-*` set, none of them OpenClaw-specific). `matches()` runs
//! two short-circuit body fingerprints:
//!
//!   Path A (main):     `tools[]` contains ≥2 of OpenClaw's RPC names
//!                      (`sessions_spawn`, `sessions_yield`, `subagents`,
//!                      `sessions_list`, `sessions_history`, `sessions_send`,
//!                      `session_status`).
//!   Path B (aux):      system prompt starts with
//!                      `"You are a context summarization assistant."` AND
//!                      `tools` is explicitly an empty array. Two anchors
//!                      AND'd because either alone is too generic.
//!
//! `is_auxiliary` mirrors `claude_cli`: under the `matches()` invariant the
//! main path always carries non-empty `tools`, so `tools.is_empty()`
//! uniquely identifies the aux path. This drops OpenClaw's deterministic
//! summary-checkpoint calls — they were collapsing 3+ unrelated sessions
//! into one fake `gen-*` session because their first-user-text and
//! first-assistant-text were byte-identical boilerplate, hashing to the
//! same fingerprint.
//!
//! User-input extraction is the trimmed last user-message text — no
//! `Sender (untrusted metadata)` scaffolding strip. The preamble is part
//! of OpenClaw's protocol surface and stripping it would couple this
//! profile to a specific wrapper format.

use crate::agent_primitives::{AgentPrimitives, SystemPromptMarkers};
use crate::profile::{AgentProfile, CallCtx, SessionIdExtraction};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;
use serde_json::Value;

use super::session_id::compose_session_id_tracked;

pub struct OpenClawProfile;

const AGENT_NAME: &str = "openclaw";

const MARKER_TOOLS: &[&str] = &[
    "sessions_spawn",
    "sessions_yield",
    "subagents",
    "sessions_list",
    "sessions_history",
    "sessions_send",
    "session_status",
];
const MARKER_HITS_REQUIRED: usize = 2;

const SUMMARIZER_PREFIX: &str = "You are a context summarization assistant.";

// Body shape parsing lives in `wire_apis::{anthropic, openai::chat}` —
// shared between every profile that classifies these wire APIs (currently
// `openclaw` here and `agents::generic`).

fn parse_tool_names(wire_api: &str, req: &Value) -> Option<Vec<String>> {
    match wire_api {
        wa::ANTHROPIC => wa::anthropic::tool_names(req),
        wa::OPENAI_CHAT => wa::openai::chat::tool_names(req),
        wa::OPENAI_RESPONSES => wa::openai::responses::tool_names(req),
        _ => None,
    }
}

fn parse_first_system_text(wire_api: &str, req: &Value) -> Option<String> {
    match wire_api {
        wa::ANTHROPIC => wa::anthropic::first_system_text(req),
        wa::OPENAI_CHAT => wa::openai::chat::first_system_text(req),
        wa::OPENAI_RESPONSES => wa::openai::responses::first_system_text(req),
        _ => None,
    }
}

impl AgentProfile for OpenClawProfile {
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

        // Path A: tools array carries ≥2 OpenClaw RPC marker names.
        if let Some(names) = parse_tool_names(ctx.call.wire_api, req) {
            let hits = MARKER_TOOLS
                .iter()
                .filter(|m| names.iter().any(|n| n == *m))
                .count();
            if hits >= MARKER_HITS_REQUIRED {
                return true;
            }
        }

        // Path B: summarizer system prompt AND tools either absent or
        // empty. Both anchors required — either alone is too generic.
        // `parse_tool_names` returns `Some(vec![])` for both "field
        // absent" and "field present but []", which is what we want
        // here: real OpenClaw summarizer calls omit the `tools` field
        // entirely.
        let tools_empty = parse_tool_names(ctx.call.wire_api, req)
            .map(|t| t.is_empty())
            .unwrap_or(false);
        if !tools_empty {
            return false;
        }
        let Some(sys) = parse_first_system_text(ctx.call.wire_api, req) else {
            return false;
        };
        sys.starts_with(SUMMARIZER_PREFIX)
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

    fn is_auxiliary(&self, ctx: &CallCtx<'_>) -> bool {
        // Under matches(): main path → ≥2 marker tools (so non-empty),
        // aux path → tools absent or []. So `tools.is_empty()` (which
        // also covers the absent case via `parse_tool_names`) uniquely
        // identifies the aux path here. `None` means the body had a
        // weird `tools` shape — be conservative and treat as non-aux.
        let Some(req) = ctx.req else {
            return false;
        };
        match parse_tool_names(ctx.call.wire_api, req) {
            Some(t) => t.is_empty(),
            None => false,
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
        // OpenAI Responses' wire-api `status: "completed"` is unreliable
        // (always present even on tool-roundtrip pending), so inspect the
        // response body directly — same reasoning as `CodexCliProfile` and
        // `GenericProfile`. Anthropic and OpenAI Chat fall through to the
        // trait-default implicit-path dispatch (duplicated here because
        // traits have no `super` to call).
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

    // `subagent` left at trait default (None). OpenClaw exposes
    // `sessions_spawn` → sub-agent dispatches are possible, but the current
    // pcap fixtures don't include any. When sub-agent traffic is captured,
    // expected fingerprint is the user's `Sender` JSON `label` field
    // differing from `"openclaw-tui"`.

    fn extract_primitives(&self, ctx: &CallCtx<'_>) -> AgentPrimitives {
        let mut p = AgentPrimitives::default();
        if let Some(req) = ctx.req {
            // Tool definitions: Anthropic flat (tools[].name) or
            // OpenAI Chat/Responses nested (tools[].function.name or flat tools[].name)
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
            // Anthropic: tool_use blocks in messages[].content[]
            if let Some(messages) = req.get("messages").and_then(|m| m.as_array()) {
                for msg in messages {
                    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                p.tool_call_count += 1;
                                if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                                    if !p.tool_names.iter().any(|n| n == name) {
                                        p.tool_names.push(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                    // OpenAI Chat: tool_calls in messages[]
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
                }
            }
            // OpenAI Responses: function_call items in input[]
            if let Some(items) = req.get("input").and_then(|i| i.as_array()) {
                for item in items {
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        p.tool_call_count += 1;
                        if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                            if !p.tool_names.iter().any(|n| n == name) {
                                p.tool_names.push(name.to_string());
                            }
                        }
                    }
                }
            }
            // System prompt markers
            if let Some(system) = req.get("system") {
                let text = match system {
                    Value::String(s) => s.clone(),
                    Value::Array(blocks) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                };
                if !text.is_empty() {
                    p.has_system_prompt = true;
                    let lower = text.to_lowercase();
                    if lower.contains("you are an agent") || lower.contains("you are claude code") {
                        p.system_prompt_markers |= SystemPromptMarkers::AGENT_LOOP;
                    }
                    if lower.contains("mcp server") || lower.contains("mcp tool") {
                        p.system_prompt_markers |= SystemPromptMarkers::MCP_SERVER;
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
        })
    }

    /// Anthropic main-path body: marker-tool-rich, Sender-prefixed user text.
    fn ant_main_body(user_text: &str) -> String {
        format!(
            r#"{{
              "model":"glm-5",
              "system":[{{"type":"text","text":"You are a personal assistant running inside OpenClaw."}}],
              "messages":[{{"role":"user","content":[{{"type":"text","text":{user_text}}}]}}],
              "tools":[
                {{"name":"read"}},{{"name":"write"}},{{"name":"edit"}},{{"name":"exec"}},
                {{"name":"sessions_spawn"}},{{"name":"subagents"}},{{"name":"session_status"}}
              ]
            }}"#,
            user_text = serde_json::to_string(user_text).unwrap()
        )
    }

    fn ant_aux_body() -> &'static str {
        r#"{
          "model":"glm-5",
          "system":[{"type":"text","text":"You are a context summarization assistant. Read the conversation."}],
          "messages":[{"role":"user","content":[{"type":"text","text":"<conversation>\n\n</conversation>"}]}],
          "tools":[]
        }"#
    }

    /// OpenAI Chat main-path body.
    fn oai_main_body(user_text: &str) -> String {
        format!(
            r#"{{
              "model":"glm-5",
              "messages":[
                {{"role":"system","content":"You are a personal assistant running inside OpenClaw."}},
                {{"role":"user","content":{user_text}}}
              ],
              "tools":[
                {{"type":"function","function":{{"name":"read"}}}},
                {{"type":"function","function":{{"name":"sessions_spawn"}}}},
                {{"type":"function","function":{{"name":"subagents"}}}}
              ]
            }}"#,
            user_text = serde_json::to_string(user_text).unwrap()
        )
    }

    fn oai_aux_body() -> &'static str {
        r#"{
          "model":"glm-5",
          "messages":[
            {"role":"system","content":"You are a context summarization assistant. Read the conversation."},
            {"role":"user","content":"<conversation>\n\n</conversation>"}
          ],
          "tools":[]
        }"#
    }

    /// OpenAI Responses main-path body: marker tools are flat
    /// `{type:"function", name, parameters}` (no nested `function`),
    /// system prompt sits in `input[]` as `role=system|developer`,
    /// and `input` is item-oriented.
    fn responses_main_body(user_text: &str) -> String {
        format!(
            r#"{{
              "model":"glm-5",
              "input":[
                {{"type":"message","role":"developer","content":[{{"type":"input_text","text":"You are a personal assistant running inside OpenClaw."}}]}},
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":{user_text}}}]}}
              ],
              "tools":[
                {{"type":"function","name":"read","parameters":{{}}}},
                {{"type":"function","name":"sessions_spawn","parameters":{{}}}},
                {{"type":"function","name":"subagents","parameters":{{}}}}
              ]
            }}"#,
            user_text = serde_json::to_string(user_text).unwrap()
        )
    }

    fn responses_aux_body() -> &'static str {
        r#"{
          "model":"glm-5",
          "input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"You are a context summarization assistant. Read the conversation."}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"<conversation>\n\n</conversation>"}]}
          ],
          "tools":[]
        }"#
    }

    // ── matches(): main path ────────────────────────────────────────────

    #[test]
    fn matches_main_anthropic_with_marker_tools() {
        let c = call(wa::ANTHROPIC, Some(&ant_main_body("hi")), None);
        assert!(OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn matches_main_openai_chat_with_marker_tools() {
        let c = call(wa::OPENAI_CHAT, Some(&oai_main_body("hi")), None);
        assert!(OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn matches_main_openai_responses_with_marker_tools() {
        let c = call(wa::OPENAI_RESPONSES, Some(&responses_main_body("hi")), None);
        assert!(OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_one_marker_tool() {
        // Only `sessions_spawn` is a marker; need ≥2.
        let body = r#"{
          "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
          "tools":[{"name":"read"},{"name":"sessions_spawn"},{"name":"write"}]
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert!(!OpenClawProfile.matches(&c.ctx()));
    }

    // ── matches(): aux path ─────────────────────────────────────────────

    #[test]
    fn matches_aux_anthropic_summarizer_empty_tools() {
        let c = call(wa::ANTHROPIC, Some(ant_aux_body()), None);
        assert!(OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn matches_aux_openai_chat_summarizer_empty_tools() {
        let c = call(wa::OPENAI_CHAT, Some(oai_aux_body()), None);
        assert!(OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn matches_aux_openai_responses_summarizer_empty_tools() {
        let c = call(wa::OPENAI_RESPONSES, Some(responses_aux_body()), None);
        assert!(OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_summarizer_with_tools_present() {
        // Path B requires explicitly-empty tools — non-empty tools that
        // happen to lack 2 markers do NOT fall through to summarizer match.
        let body = r#"{
          "system":[{"type":"text","text":"You are a context summarization assistant."}],
          "messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],
          "tools":[{"name":"read"}]
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert!(!OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn matches_aux_when_tools_field_missing_anthropic() {
        // Real OpenClaw summarizer calls omit the `tools` field entirely.
        // Path B treats absent same as `[]` — summarizer prompt + no
        // tools is the aux fingerprint.
        let body = r#"{
          "system":[{"type":"text","text":"You are a context summarization assistant."}],
          "messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert!(OpenClawProfile.matches(&c.ctx()));
        assert!(OpenClawProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn does_not_match_when_tools_field_has_wrong_shape() {
        // `tools: 5` (scalar) → unparseable shape, conservative: not openclaw.
        let body = r#"{
          "system":[{"type":"text","text":"You are a context summarization assistant."}],
          "messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],
          "tools": 5
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert!(!OpenClawProfile.matches(&c.ctx()));
    }

    // ── matches(): negative ──────────────────────────────────────────────

    #[test]
    fn does_not_match_unknown_wire_api() {
        // Defensive: when a future wire-api is added that openclaw hasn't
        // been taught to recognize, matches() must short-circuit before
        // trying to parse the body shape.
        let mut c = call(wa::ANTHROPIC, Some(&ant_main_body("hi")), None);
        c.call.wire_api = "future-wire-api";
        assert!(!OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_unrelated_anthropic_traffic() {
        let body = r#"{
          "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
          "tools":[{"name":"Read"},{"name":"Edit"}]
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert!(!OpenClawProfile.matches(&c.ctx()));
    }

    #[test]
    fn does_not_match_garbage_body() {
        let c = call(wa::ANTHROPIC, Some("not json"), None);
        assert!(!OpenClawProfile.matches(&c.ctx()));
    }

    // ── is_auxiliary ────────────────────────────────────────────────────

    #[test]
    fn is_auxiliary_true_aux_path_anthropic() {
        let c = call(wa::ANTHROPIC, Some(ant_aux_body()), None);
        assert!(OpenClawProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_true_aux_path_openai_chat() {
        let c = call(wa::OPENAI_CHAT, Some(oai_aux_body()), None);
        assert!(OpenClawProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_false_main_path() {
        let c = call(wa::ANTHROPIC, Some(&ant_main_body("hi")), None);
        assert!(!OpenClawProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_true_when_tools_field_missing() {
        // Real OpenClaw summarizer bodies omit `tools` entirely, so
        // `is_auxiliary` must treat absent same as empty — anything else
        // would re-introduce the `gen-*` collision bug. Caller is
        // expected to have passed `matches()` first; an unmatched body
        // happening to lack tools shouldn't reach this point in
        // production.
        let body = r#"{
          "messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert!(OpenClawProfile.is_auxiliary(&c.ctx()));
    }

    #[test]
    fn is_auxiliary_false_when_tools_field_has_wrong_shape() {
        // `tools: 5` (scalar) → unparseable; conservative non-aux verdict.
        let body = r#"{
          "messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],
          "tools": "not-an-array"
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert!(!OpenClawProfile.is_auxiliary(&c.ctx()));
    }

    // ── extract_user_input ──────────────────────────────────────────────

    #[test]
    fn extract_user_input_returns_last_user_text_anthropic() {
        let c = call(wa::ANTHROPIC, Some(&ant_main_body("plain question")), None);
        assert_eq!(
            OpenClawProfile.extract_user_input(&c.ctx()).as_deref(),
            Some("plain question"),
        );
    }

    #[test]
    fn extract_user_input_returns_last_user_text_openai_chat() {
        let c = call(wa::OPENAI_CHAT, Some(&oai_main_body("check status")), None);
        assert_eq!(
            OpenClawProfile.extract_user_input(&c.ctx()).as_deref(),
            Some("check status"),
        );
    }

    // ── extract_session_id ──────────────────────────────────────────────

    #[test]
    fn extract_session_id_anthropic_main_uses_first_tool_id_from_response() {
        let req = ant_main_body("hi");
        let resp = r#"{"content":[{"type":"tool_use","id":"toolu_abc","name":"exec","input":{}}]}"#;
        let c = call(wa::ANTHROPIC, Some(&req), Some(resp));
        let ids = OpenClawProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, "toolu_abc");
    }

    #[test]
    fn extract_session_id_openai_chat_main_uses_first_tool_id_from_response() {
        let req = oai_main_body("hi");
        let resp = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call_abc","type":"function","function":{"name":"exec","arguments":"{}"}}]}}]}"#;
        let c = call(wa::OPENAI_CHAT, Some(&req), Some(resp));
        let ids = OpenClawProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, "call_abc");
    }

    #[test]
    fn extract_session_id_openai_responses_main_uses_first_function_call_id_from_response() {
        let req = responses_main_body("hi");
        let resp = r#"{"output":[{"type":"function_call","name":"exec","arguments":"{}","call_id":"fc_abc"}]}"#;
        let c = call(wa::OPENAI_RESPONSES, Some(&req), Some(resp));
        let ids = OpenClawProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, "fc_abc");
    }

    #[test]
    fn extract_session_id_canonicalizes_stripped_underscore() {
        // OpenClaw quirk: client echoes `callabc` (no underscore) in
        // assistant.tool_calls history. Canonicalizer restores `call_abc`.
        let req = r#"{
          "messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"callabc","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"callabc","content":"ok"}
          ],
          "tools":[
            {"type":"function","function":{"name":"sessions_spawn"}},
            {"type":"function","function":{"name":"subagents"}}
          ]
        }"#;
        let c = call(wa::OPENAI_CHAT, Some(req), None);
        let ids = OpenClawProfile.extract_session_id(&c.ctx()).unwrap();
        assert_eq!(ids.session_id, "call_abc");
        assert!(ids.tool_id_canonicalized);
    }

    // ── is_user_turn_start ──────────────────────────────────────────────

    #[test]
    fn is_user_turn_start_text_user_anthropic() {
        let c = call(wa::ANTHROPIC, Some(&ant_main_body("hi")), None);
        assert_eq!(OpenClawProfile.is_user_turn_start(&c.ctx()), Some(true));
    }

    #[test]
    fn is_user_turn_start_false_for_tool_result_only_anthropic() {
        let body = r#"{
          "messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t","name":"exec","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}
          ],
          "tools":[{"name":"sessions_spawn"},{"name":"subagents"}]
        }"#;
        let c = call(wa::ANTHROPIC, Some(body), None);
        assert_eq!(OpenClawProfile.is_user_turn_start(&c.ctx()), Some(false));
    }

    #[test]
    fn is_user_turn_start_false_when_last_role_is_tool_openai_chat() {
        let body = r#"{
          "messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_a","content":"ok"}
          ],
          "tools":[{"type":"function","function":{"name":"sessions_spawn"}},{"type":"function","function":{"name":"subagents"}}]
        }"#;
        let c = call(wa::OPENAI_CHAT, Some(body), None);
        assert_eq!(OpenClawProfile.is_user_turn_start(&c.ctx()), Some(false));
    }

    #[test]
    fn is_user_turn_start_false_when_last_is_function_call_output_openai_responses() {
        // Responses tool roundtrips emit function_call_output items at the
        // tail; that's a continuation, not a user turn start.
        let body = r#"{
          "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"function_call","name":"exec","arguments":"{}","call_id":"fc_a"},
            {"type":"function_call_output","call_id":"fc_a","output":"ok"}
          ],
          "tools":[{"type":"function","name":"sessions_spawn"},{"type":"function","name":"subagents"}]
        }"#;
        let c = call(wa::OPENAI_RESPONSES, Some(body), None);
        assert_eq!(OpenClawProfile.is_user_turn_start(&c.ctx()), Some(false));
    }

    #[test]
    fn is_turn_terminal_openai_responses_uses_body_only_check() {
        // Responses' wire-api `status: "completed"` is unreliable, so
        // openclaw inspects the response body — `message`-only output is
        // terminal, presence of any `*_call` is not.
        let wires = crate::wire_apis::build_default_wire_api_registry();
        let resp_terminal = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}"#;
        let mut c = call(wa::OPENAI_RESPONSES, None, Some(resp_terminal));
        assert!(OpenClawProfile.is_turn_terminal(&c.ctx(), &wires));
        c.set_response_body(
            r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#,
        );
        assert!(!OpenClawProfile.is_turn_terminal(&c.ctx(), &wires));
    }

    // ── extract_assistant_text ──────────────────────────────────────────

    #[test]
    fn extract_assistant_text_anthropic_joins_text_skips_thinking_and_tool_use() {
        let resp = r#"{"content":[
            {"type":"thinking","thinking":"internal"},
            {"type":"text","text":"part one"},
            {"type":"tool_use","id":"t","name":"exec","input":{}},
            {"type":"text","text":"part two"}
        ]}"#;
        let c = call(wa::ANTHROPIC, None, Some(resp));
        assert_eq!(
            OpenClawProfile.extract_assistant_text(&c.ctx()).as_deref(),
            Some("part one\npart two"),
        );
    }

    #[test]
    fn extract_assistant_text_openai_chat_from_choices_message_content() {
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello world"}}]}"#;
        let c = call(wa::OPENAI_CHAT, None, Some(resp));
        assert_eq!(
            OpenClawProfile.extract_assistant_text(&c.ctx()).as_deref(),
            Some("hello world"),
        );
    }

    #[test]
    fn extract_assistant_text_none_when_only_tool_calls() {
        let resp = r#"{"content":[{"type":"tool_use","id":"t","name":"exec","input":{}}]}"#;
        let c = call(wa::ANTHROPIC, None, Some(resp));
        assert!(OpenClawProfile.extract_assistant_text(&c.ctx()).is_none());
    }
}
