//! Generic agent profile — synthesizes a session id from request / response
//! payload tool-call anchors, so Heron can still produce `AgentTurn`s for
//! header-less tool-using LLM traffic across supported wire APIs.
//!
//! `agent_kind == "generic"` is wire-api-agnostic; downstream consumers read
//! `wire_api` separately. Per-shape parsing lives in `wire_apis::{anthropic,
//! openai::chat, openai::responses}` and is shared with `openclaw`; the
//! profile is a thin dispatcher on `call.wire_api`.

use crate::agent_primitives::{AgentPrimitives, SystemPromptMarkers};
use crate::profile::{AgentProfile, CallCtx, SessionIdExtraction};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;

use super::session_id::compose_session_id_tracked;
use crate::wire_apis::AssistantSig;

pub struct GenericProfile;

fn extract_tool_anchored_sig(ctx: &CallCtx<'_>) -> Option<(String, AssistantSig)> {
    let req = ctx.req?;
    // Track three pieces per wire api:
    //   * user_text   — first user message; required to compose stable
    //                   tool-call session ids.
    //   * sig_in_req  — Some(sig) if an assistant message already lives
    //                   inside `req.messages[*]` / `req.input[*]`. This
    //                   tells us the call belongs to a multi-turn
    //                   conversation; the caller has already established
    //                   a stable anchor we can hash.
    //   * sig_in_resp — Some(sig) only when sig_in_req is None and the
    //                   response carries the assistant turn for the
    //                   first user message.
    let (user_text, sig_in_req, sig_in_resp) = match ctx.call.wire_api {
        wa::ANTHROPIC => {
            let user_text = wa::anthropic::first_user_text(req)?;
            let sig_in_req = wa::anthropic::first_assistant_sig_from_request(req);
            let sig_in_resp = if sig_in_req.is_none() {
                ctx.resp
                    .and_then(wa::anthropic::first_assistant_sig_from_response_value)
            } else {
                None
            };
            (user_text, sig_in_req, sig_in_resp)
        }
        wa::OPENAI_CHAT => {
            let user_text = wa::openai::chat::first_user_text(req)?;
            let sig_in_req = wa::openai::chat::first_assistant_sig_from_request(req);
            let sig_in_resp = if sig_in_req.is_none() {
                ctx.resp
                    .and_then(wa::openai::chat::first_assistant_sig_from_response_value)
            } else {
                None
            };
            (user_text, sig_in_req, sig_in_resp)
        }
        wa::OPENAI_RESPONSES => {
            let (user_text, sig_in_req) = match req.get("input")? {
                serde_json::Value::Array(items) => (
                    wa::openai::responses::first_user_text(items),
                    wa::openai::responses::first_assistant_sig_from_input(items),
                ),
                serde_json::Value::String(s) if !s.trim().is_empty() => (Some(s.clone()), None),
                _ => (None, None),
            };
            let user_text = user_text?;
            let sig_in_resp = if sig_in_req.is_none() {
                ctx.resp
                    .and_then(wa::openai::responses::first_assistant_sig_from_response_value)
            } else {
                None
            };
            (user_text, sig_in_req, sig_in_resp)
        }
        wa::GEMINI_AISTUDIO => {
            let user_text = wa::gemini_aistudio::first_user_text(req)?;
            let sig_in_req = wa::gemini_aistudio::first_assistant_sig_from_request(req);
            let sig_in_resp = if sig_in_req.is_none() {
                ctx.resp
                    .and_then(wa::gemini_aistudio::first_assistant_sig_from_response_value)
            } else {
                None
            };
            (user_text, sig_in_req, sig_in_resp)
        }
        _ => return None,
    };

    let sig = sig_in_req.or(sig_in_resp)?;
    if !matches!(sig, AssistantSig::ToolId(_)) {
        return None;
    }
    Some((user_text, sig))
}

impl AgentProfile for GenericProfile {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn matches(&self, ctx: &CallCtx<'_>) -> bool {
        matches!(
            ctx.call.wire_api,
            wa::ANTHROPIC | wa::OPENAI_CHAT | wa::OPENAI_RESPONSES | wa::GEMINI_AISTUDIO
        ) && extract_tool_anchored_sig(ctx).is_some()
    }

    fn extract_session_id(&self, ctx: &CallCtx<'_>) -> Option<SessionIdExtraction> {
        let (user_text, sig) = extract_tool_anchored_sig(ctx)?;
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
            wa::GEMINI_AISTUDIO => wa::gemini_aistudio::is_user_turn_start(req),
            _ => None,
        }
    }

    fn extract_user_input(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let req = ctx.req?;
        match ctx.call.wire_api {
            wa::ANTHROPIC => wa::anthropic::extract_user_input(req),
            wa::OPENAI_CHAT => wa::openai::chat::extract_user_input(req),
            wa::OPENAI_RESPONSES => wa::openai::responses::extract_user_input(req),
            wa::GEMINI_AISTUDIO => wa::gemini_aistudio::extract_user_input(req),
            _ => None,
        }
    }

    fn extract_assistant_text(&self, ctx: &CallCtx<'_>) -> Option<String> {
        let resp = ctx.resp?;
        match ctx.call.wire_api {
            wa::ANTHROPIC => wa::anthropic::extract_assistant_text_value(resp),
            wa::OPENAI_CHAT => wa::openai::chat::extract_assistant_text_value(resp),
            wa::OPENAI_RESPONSES => wa::openai::responses::extract_assistant_text_value(resp),
            wa::GEMINI_AISTUDIO => wa::gemini_aistudio::extract_assistant_text_value(resp),
            _ => None,
        }
    }

    fn is_turn_terminal(&self, ctx: &CallCtx<'_>, wire_apis: &WireApiRegistry) -> bool {
        // OpenAI Responses' wire-api `status: "completed"` is unreliable
        // (always present even on tool-roundtrip pending), so inspect the
        // response body directly — same reasoning as `CodexCliProfile`.
        // Anthropic and OpenAI Chat fall through to the trait-default
        // implicit-path dispatch (duplicated here because traits have no
        // `super` to call).
        if ctx.call.wire_api == wa::OPENAI_RESPONSES {
            match ctx.resp {
                Some(resp) => crate::wire_apis::openai::body_has_terminal_message_only_value(resp),
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

    fn extract_primitives(&self, ctx: &CallCtx<'_>) -> AgentPrimitives {
        let mut p = AgentPrimitives::default();
        if let Some(req) = ctx.req {
            // Best-effort: try Anthropic shape (messages[].content[].type=="tool_use")
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
                    // OpenAI Chat shape: messages[].tool_calls[]
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
            // System prompt: Anthropic top-level "system" string
            if let Some(system) = req.get("system").and_then(|s| s.as_str()) {
                if !system.is_empty() {
                    p.has_system_prompt = true;
                    let lower = system.to_lowercase();
                    if lower.contains("you are an agent") || lower.contains("you are claude code") {
                        p.system_prompt_markers |= SystemPromptMarkers::AGENT_LOOP;
                    }
                    if lower.contains("mcp server") || lower.contains("mcp tool") {
                        p.system_prompt_markers |= SystemPromptMarkers::MCP_SERVER;
                    }
                }
            }
            // System prompt: OpenAI Chat messages[role=system].content
            if let Some(messages) = req.get("messages").and_then(|m| m.as_array()) {
                for msg in messages {
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
                                if lower.contains("mcp server") || lower.contains("mcp tool") {
                                    p.system_prompt_markers |= SystemPromptMarkers::MCP_SERVER;
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

// ─────────────────────────────── Tests ──────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use crate::profile::TestCall;
    use std::net::IpAddr;

    fn call_with(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        req: Option<&str>,
        resp: Option<&str>,
    ) -> TestCall {
        call_with_finish(wire_api, headers, req, resp, None)
    }

    fn call_with_finish(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        req: Option<&str>,
        resp: Option<&str>,
        finish_reason: Option<&str>,
    ) -> TestCall {
        let path = match wire_api {
            wa::ANTHROPIC => "/v1/messages",
            wa::OPENAI_CHAT => "/v1/chat/completions",
            wa::OPENAI_RESPONSES => "/v1/responses",
            wa::GEMINI_AISTUDIO => "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            _ => "/",
        };
        TestCall::new(LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "m".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: path.into(),
            is_stream: true,
            request_body: req.map(str::to_string),
            status_code: None,
            finish_reason: finish_reason.map(str::to_string),
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
            request_headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            response_headers: vec![],
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
        })
    }

    // ── Cross-wire-api: matches() ───────────────────────────────────────────

    #[test]
    fn matches_supported_wire_apis_with_tool_anchor() {
        let cases = [
            (
                wa::ANTHROPIC,
                r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
                r#"{"content":[{"type":"tool_use","id":"toolu_a","name":"Read","input":{}}]}"#,
            ),
            (
                wa::OPENAI_CHAT,
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            ),
            (
                wa::OPENAI_RESPONSES,
                r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
                r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#,
            ),
            (
                wa::GEMINI_AISTUDIO,
                r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]}}]}"#,
            ),
        ];
        for &(wire, req, resp) in &cases {
            let c = call_with(wire, vec![], Some(req), Some(resp));
            assert!(GenericProfile.matches(&c.ctx()), "should match {wire}");
        }
    }

    #[test]
    fn does_not_match_plain_text_openai_chat() {
        let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let c = call_with(wa::OPENAI_CHAT, vec![], Some(req), Some(resp));
        assert!(!GenericProfile.matches(&c.ctx()));
    }

    // ───────────────────── Gemini AI Studio (gemini-aistudio) ──────────────
    mod gemini_aistudio_wire {
        use super::*;

        fn g(req: Option<&str>, resp: Option<&str>) -> TestCall {
            call_with(wa::GEMINI_AISTUDIO, vec![], req, resp)
        }

        #[test]
        fn extract_session_id_call_n_with_model_history() {
            // Multi-turn: contents has a prior model turn → sig from request
            // anchors session_id; resp not consulted.
            let req = r#"{
                "contents": [
                    {"role":"user","parts":[{"text":"<session_context>session"}]},
                    {"role":"user","parts":[{"text":"original question"}]},
                    {"role":"model","parts":[
                        {"text":"I'll search."},
                        {"functionCall":{"name":"grep","args":{"q":"x"}}}
                    ]},
                    {"role":"user","parts":[
                        {"functionResponse":{"name":"grep","response":{"hits":1}}}
                    ]}
                ]
            }"#;
            let c = g(Some(req), None);
            let extracted = GenericProfile.extract_session_id(&c.ctx());
            assert!(
                extracted.is_some(),
                "should extract session_id from request history"
            );
        }

        #[test]
        fn extract_session_id_call_one_uses_response_sig() {
            // First call: no model turn in contents yet → fall back to response sig.
            let req = r#"{
                "contents": [
                    {"role":"user","parts":[{"text":"<session_context>scaffold"}]},
                    {"role":"user","parts":[{"text":"hello"}]}
                ]
            }"#;
            let resp = r#"{
                "candidates": [{
                    "content":{"role":"model","parts":[
                        {"text":"hi there"},
                        {"functionCall":{"name":"f","args":{"x":1}}}
                    ]}
                }]
            }"#;
            let c = g(Some(req), Some(resp));
            let extracted = GenericProfile.extract_session_id(&c.ctx());
            assert!(extracted.is_some(), "should fall back to response sig");
        }

        #[test]
        fn extract_user_input_skips_function_response_continuation() {
            let req = r#"{
                "contents": [
                    {"role":"user","parts":[{"text":"<session_context>scaffold"}]},
                    {"role":"user","parts":[{"text":"the real question"}]},
                    {"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]},
                    {"role":"user","parts":[
                        {"functionResponse":{"name":"f","response":{"r":1}}}
                    ]}
                ]
            }"#;
            let c = g(Some(req), None);
            assert_eq!(
                GenericProfile.extract_user_input(&c.ctx()).as_deref(),
                Some("the real question"),
            );
        }

        #[test]
        fn is_user_turn_start_false_on_function_response_only_continuation() {
            let req = r#"{
                "contents": [
                    {"role":"user","parts":[{"text":"hi"}]},
                    {"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]},
                    {"role":"user","parts":[
                        {"functionResponse":{"name":"f","response":{}}}
                    ]}
                ]
            }"#;
            let c = g(Some(req), None);
            assert_eq!(GenericProfile.is_user_turn_start(&c.ctx()), Some(false));
        }

        // ── Pcap ground-truth: 7 calls → 2 turns of 4 + 3 ────────────────
        //
        // Mirrors the 7 LlmCalls captured in
        // `~/Downloads/gemini-cli-apikey.pcap`. Each call's `contents` field
        // is the actual sequence the wire transmitted (text/functionCall/
        // functionResponse types preserved; bodies abbreviated for legibility
        // but structurally faithful — the text bytes themselves don't drive
        // any turn-boundary decision).
        //
        // Expected turn split per the user-confirmed ground truth:
        //   Turn A = calls 1..=4   (initial prompt + tool roundtrips,
        //                            closes when call-4's model response
        //                            is pure text — no functionCall — so
        //                            finish_reason stays STOP.)
        //   Turn B = calls 5..=7   (user's follow-up at call-5,
        //                            then two more tool roundtrips.)
        //
        // We verify three load-bearing predicates that drive turn assembly:
        //   1. session_id is identical for all 7 calls (proves sig algo
        //      stays consistent across the resp-vs-req-history boundary).
        //   2. is_user_turn_start = [T, F, F, F, T, F, F] — boundaries
        //      only on calls 1 and 5.
        //   3. is_turn_terminal = [F, F, F, T, F, F, T] — call 4 closes
        //      turn A (model's response was pure text), call 7 closes
        //      turn B at end of session.

        // For brevity, abbreviate the long shared scaffolding to short
        // strings — the sig algorithm does not care about content length,
        // only structural identity across the resp-vs-req-echo boundary.
        // What matters: identical bytes for the same content across calls.
        // The real pcap also carries a 24KB systemInstruction (Gemini CLI
        // persona). We include a non-empty system here on every request so
        // call 1 must still anchor on the response-side functionCall instead
        // of being mistaken for a plain text one-shot.
        const SYSTEM_PROMPT: &str = "You are an interactive CLI agent.";
        const SESSION_CTX: &str = "<session_context>scaffold";
        const ORIGINAL_PROMPT: &str = "original prompt about h-turn";
        const FOLLOWUP_PROMPT: &str = "user follow-up at call 5";
        const MODEL_TURN_1_TEXT: &str = "I'll investigate.";
        const MODEL_TURN_3_TEXT: &str = "Got it. Asking user.";
        const MODEL_TURN_4_TEXT: &str = "Final analysis text only — no tool call.";
        const MODEL_TURN_5_TEXT: &str = "Understood. Continuing.";
        const MODEL_TURN_6_TEXT: &str = "Refining further.";

        // Build conversation prefix incrementally — each call's contents
        // is a strict extension of the previous call's. Mirrors how
        // Gemini CLI accumulates `contents[]` across roundtrips.
        fn req_call_1() -> String {
            format!(
                r#"{{"systemInstruction":{{"role":"user","parts":[{{"text":"{SYSTEM_PROMPT}"}}]}},"contents":[
                    {{"role":"user","parts":[{{"text":"{SESSION_CTX}"}}]}},
                    {{"role":"user","parts":[{{"text":"{ORIGINAL_PROMPT}"}}]}}
                ]}}"#
            )
        }
        fn req_call_2() -> String {
            format!(
                r#"{{"systemInstruction":{{"role":"user","parts":[{{"text":"{SYSTEM_PROMPT}"}}]}},"contents":[
                    {{"role":"user","parts":[{{"text":"{SESSION_CTX}"}}]}},
                    {{"role":"user","parts":[{{"text":"{ORIGINAL_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_1_TEXT}"}},
                        {{"functionCall":{{"name":"grep","args":{{"q":"x"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"a"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"b"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"grep","response":{{"r":1}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":2}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":3}}}}}}
                    ]}}
                ]}}"#
            )
        }
        fn req_call_3() -> String {
            format!(
                r#"{{"systemInstruction":{{"role":"user","parts":[{{"text":"{SYSTEM_PROMPT}"}}]}},"contents":[
                    {{"role":"user","parts":[{{"text":"{SESSION_CTX}"}}]}},
                    {{"role":"user","parts":[{{"text":"{ORIGINAL_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_1_TEXT}"}},
                        {{"functionCall":{{"name":"grep","args":{{"q":"x"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"a"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"b"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"grep","response":{{"r":1}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":2}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":3}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"functionCall":{{"name":"read","args":{{"f":"c"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":4}}}}}}
                    ]}}
                ]}}"#
            )
        }
        fn req_call_4() -> String {
            format!(
                r#"{{"systemInstruction":{{"role":"user","parts":[{{"text":"{SYSTEM_PROMPT}"}}]}},"contents":[
                    {{"role":"user","parts":[{{"text":"{SESSION_CTX}"}}]}},
                    {{"role":"user","parts":[{{"text":"{ORIGINAL_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_1_TEXT}"}},
                        {{"functionCall":{{"name":"grep","args":{{"q":"x"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"a"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"b"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"grep","response":{{"r":1}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":2}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":3}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"functionCall":{{"name":"read","args":{{"f":"c"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":4}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_3_TEXT}"}},
                        {{"functionCall":{{"name":"ask_user","args":{{"q":"ok?"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"ask_user","response":{{"a":"yes"}}}}}}
                    ]}}
                ]}}"#
            )
        }
        // Call 5: user fires a follow-up text — this is the new turn boundary.
        fn req_call_5() -> String {
            format!(
                r#"{{"systemInstruction":{{"role":"user","parts":[{{"text":"{SYSTEM_PROMPT}"}}]}},"contents":[
                    {{"role":"user","parts":[{{"text":"{SESSION_CTX}"}}]}},
                    {{"role":"user","parts":[{{"text":"{ORIGINAL_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_1_TEXT}"}},
                        {{"functionCall":{{"name":"grep","args":{{"q":"x"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"a"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"b"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"grep","response":{{"r":1}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":2}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":3}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"functionCall":{{"name":"read","args":{{"f":"c"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":4}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_3_TEXT}"}},
                        {{"functionCall":{{"name":"ask_user","args":{{"q":"ok?"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"ask_user","response":{{"a":"yes"}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_4_TEXT}"}}
                    ]}},
                    {{"role":"user","parts":[{{"text":"{FOLLOWUP_PROMPT}"}}]}}
                ]}}"#
            )
        }
        fn req_call_6() -> String {
            // Call 5 completes with model response `MODEL_TURN_5_TEXT + 2 fc`,
            // CLI echoes them back along with two functionResponses.
            format!(
                r#"{{"systemInstruction":{{"role":"user","parts":[{{"text":"{SYSTEM_PROMPT}"}}]}},"contents":[
                    {{"role":"user","parts":[{{"text":"{SESSION_CTX}"}}]}},
                    {{"role":"user","parts":[{{"text":"{ORIGINAL_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_1_TEXT}"}},
                        {{"functionCall":{{"name":"grep","args":{{"q":"x"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"a"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"b"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"grep","response":{{"r":1}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":2}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":3}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"functionCall":{{"name":"read","args":{{"f":"c"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":4}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_3_TEXT}"}},
                        {{"functionCall":{{"name":"ask_user","args":{{"q":"ok?"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"ask_user","response":{{"a":"yes"}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_4_TEXT}"}}
                    ]}},
                    {{"role":"user","parts":[{{"text":"{FOLLOWUP_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_5_TEXT}"}},
                        {{"functionCall":{{"name":"read","args":{{"f":"d"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"e"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":5}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":6}}}}}}
                    ]}}
                ]}}"#
            )
        }
        fn req_call_7() -> String {
            format!(
                r#"{{"systemInstruction":{{"role":"user","parts":[{{"text":"{SYSTEM_PROMPT}"}}]}},"contents":[
                    {{"role":"user","parts":[{{"text":"{SESSION_CTX}"}}]}},
                    {{"role":"user","parts":[{{"text":"{ORIGINAL_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_1_TEXT}"}},
                        {{"functionCall":{{"name":"grep","args":{{"q":"x"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"a"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"b"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"grep","response":{{"r":1}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":2}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":3}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"functionCall":{{"name":"read","args":{{"f":"c"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":4}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_3_TEXT}"}},
                        {{"functionCall":{{"name":"ask_user","args":{{"q":"ok?"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"ask_user","response":{{"a":"yes"}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_4_TEXT}"}}
                    ]}},
                    {{"role":"user","parts":[{{"text":"{FOLLOWUP_PROMPT}"}}]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_5_TEXT}"}},
                        {{"functionCall":{{"name":"read","args":{{"f":"d"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"e"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":5}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":6}}}}}}
                    ]}},
                    {{"role":"model","parts":[
                        {{"text":"{MODEL_TURN_6_TEXT}"}},
                        {{"functionCall":{{"name":"read","args":{{"f":"g"}}}}}},
                        {{"functionCall":{{"name":"read","args":{{"f":"h"}}}}}}
                    ]}},
                    {{"role":"user","parts":[
                        {{"functionResponse":{{"name":"read","response":{{"r":7}}}}}},
                        {{"functionResponse":{{"name":"read","response":{{"r":8}}}}}}
                    ]}}
                ]}}"#
            )
        }

        // Synthetic responses for call 1 — used as `sig_in_resp` fallback
        // when no model turn yet exists in `contents`. Subsequent calls
        // already have model history in their `contents`, so their session_id
        // derives from `sig_in_req`; we set their resp/finish_reason just to
        // exercise `is_turn_terminal`.
        fn resp_call_1() -> String {
            format!(
                r#"{{"candidates":[{{"index":0,"content":{{"role":"model","parts":[
                    {{"text":"{MODEL_TURN_1_TEXT}"}},
                    {{"functionCall":{{"name":"grep","args":{{"q":"x"}}}}}},
                    {{"functionCall":{{"name":"read","args":{{"f":"a"}}}}}},
                    {{"functionCall":{{"name":"read","args":{{"f":"b"}}}}}}
                ]}},"finishReason":"STOP"}}]}}"#
            )
        }

        #[test]
        fn pcap_seven_calls_split_into_two_turns_of_four_and_three() {
            // Each row mirrors a real LlmCall extracted from the pcap.
            // (request_body, response_body, finish_reason)
            let calls: [(String, Option<String>, &'static str); 7] = [
                // Call 1 — first prompt; turn A start.
                // Resp has functionCalls so wire-api synthesizes finish_reason=TOOL_USE.
                (req_call_1(), Some(resp_call_1()), "TOOL_USE"),
                // Calls 2..4 — tool roundtrips inside turn A.
                (req_call_2(), None, "TOOL_USE"),
                (req_call_3(), None, "TOOL_USE"),
                // Call 4 — model's response is pure text; finish_reason stays
                // STOP (no functionCall to trigger TOOL_USE synthesis), so
                // is_turn_terminal=true, closing turn A.
                (req_call_4(), None, "STOP"),
                // Call 5 — user's follow-up; turn B start.
                (req_call_5(), None, "TOOL_USE"),
                // Calls 6..7 — tool roundtrips inside turn B.
                (req_call_6(), None, "TOOL_USE"),
                (req_call_7(), None, "STOP"),
            ];

            let wires = wa::build_default_wire_api_registry();
            let mut session_ids: Vec<String> = Vec::new();
            let mut user_starts: Vec<bool> = Vec::new();
            let mut terminals: Vec<bool> = Vec::new();

            for (i, (req, resp, fr)) in calls.iter().enumerate() {
                let c = call_with_finish(
                    wa::GEMINI_AISTUDIO,
                    vec![],
                    Some(req),
                    resp.as_deref(),
                    Some(fr),
                );
                let sid = GenericProfile
                    .extract_session_id(&c.ctx())
                    .map(|x| x.session_id)
                    .unwrap_or_else(|| panic!("call {} should produce a session_id", i + 1));
                session_ids.push(sid);
                user_starts.push(GenericProfile.is_user_turn_start(&c.ctx()).unwrap_or(false));
                terminals.push(GenericProfile.is_turn_terminal(&c.ctx(), &wires));
            }

            // (1) All 7 calls land in the same session.
            for (i, sid) in session_ids.iter().enumerate().skip(1) {
                assert_eq!(
                    sid,
                    &session_ids[0],
                    "call {} session_id diverged from call 1",
                    i + 1,
                );
            }

            // (2) Turn-start boundaries: only calls 1 and 5.
            assert_eq!(
                user_starts,
                vec![true, false, false, false, true, false, false],
                "is_user_turn_start mismatched expected boundaries",
            );

            // (3) Terminal boundaries: turn A closes at call 4, turn B at call 7.
            assert_eq!(
                terminals,
                vec![false, false, false, true, false, false, true],
                "is_turn_terminal mismatched expected closures",
            );
        }
    }

    // ───────────────────── Anthropic (wire_api=anthropic) ──────────────────
    mod anthropic_wire {
        use super::*;

        fn ant(headers: Vec<(&str, &str)>, req: Option<&str>, resp: Option<&str>) -> TestCall {
            call_with(wa::ANTHROPIC, headers, req, resp)
        }

        #[test]
        fn extract_session_id_call_n_with_tool_history() {
            let req = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"ok"}]}
            ]}"#;
            let c = ant(vec![], Some(req), None);
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert_eq!(ids.session_id, "toolu_abc");
        }

        #[test]
        fn extract_session_id_none_for_text_only_history() {
            let req = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[{"type":"text","text":"hello there"}]},
                {"role":"user","content":[{"type":"text","text":"more"}]}
            ]}"#;
            let c = ant(vec![], Some(req), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_call_1_tool_in_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let resp =
                r#"{"content":[{"type":"tool_use","id":"toolu_xyz","name":"Read","input":{}}]}"#;
            let c = ant(vec![], Some(req), Some(resp));
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert_eq!(ids.session_id, "toolu_xyz");
        }

        #[test]
        fn extract_session_id_none_for_call_1_text_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let resp = r#"{"content":[{"type":"text","text":"hello there"}]}"#;
            let c = ant(vec![], Some(req), Some(resp));
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_call_1_and_n_match() {
            let resp =
                r#"{"content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]}"#;
            let req1 =
                r#"{"messages":[{"role":"user","content":[{"type":"text","text":"prompt"}]}]}"#;
            let req2 = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"prompt"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_same","content":"ok"}]}
            ]}"#;
            let c1 = ant(vec![], Some(req1), Some(resp));
            let c2 = ant(vec![], Some(req2), None);
            let id1 = GenericProfile
                .extract_session_id(&c1.ctx())
                .unwrap()
                .session_id;
            let id2 = GenericProfile
                .extract_session_id(&c2.ctx())
                .unwrap()
                .session_id;
            assert_eq!(
                id1, id2,
                "call #1 and call #2 must synthesize same session_id"
            );
        }

        #[test]
        fn extract_session_id_call_1_with_normalized_tool_id() {
            let resp =
                r#"{"content":[{"type":"tool_use","id":"toolu_abc","name":"R","input":{}}]}"#;
            let req2 = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"x"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"tooluabc","name":"R","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"tooluabc","content":"ok"}]}
            ]}"#;
            let c1 = ant(
                vec![],
                Some(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#),
                Some(resp),
            );
            let c2 = ant(vec![], Some(req2), None);
            assert_eq!(
                GenericProfile
                    .extract_session_id(&c1.ctx())
                    .unwrap()
                    .session_id,
                GenericProfile
                    .extract_session_id(&c2.ctx())
                    .unwrap()
                    .session_id,
            );
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = ant(vec![], Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn is_user_turn_start_text() {
            let req =
                r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert_eq!(GenericProfile.is_user_turn_start(&c.ctx()), Some(true));
        }

        #[test]
        fn is_user_turn_start_tool_result_only() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert_eq!(GenericProfile.is_user_turn_start(&c.ctx()), Some(false));
        }
    }

    // ─────────────────── OpenAI Chat (wire_api=openai-chat) ────────────────
    mod openai_chat_wire {
        use super::*;

        fn oai(req: Option<&str>, resp: Option<&str>) -> TestCall {
            call_with(wa::OPENAI_CHAT, vec![], req, resp)
        }

        #[test]
        fn extract_session_id_call_n_with_tool_history() {
            let req = r#"{"messages":[
                {"role":"system","content":"you are helpful"},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_abc","content":"ok"}
            ]}"#;
            let c = oai(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert_eq!(ids.session_id, "call_abc");
        }

        #[test]
        fn extract_session_id_call_1_tool_in_response_canonicalized() {
            // OpenClaw quirk: tool_id without underscore in echo, canonical in response.
            let req1 = r#"{"messages":[{"role":"user","content":"x"}]}"#;
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#;
            let req2 = r#"{"messages":[
                {"role":"user","content":"x"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"callabc","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"callabc","content":"ok"}
            ]}"#;
            let c1 = oai(Some(req1), Some(resp));
            let c2 = oai(Some(req2), None);
            let id1 = GenericProfile
                .extract_session_id(&c1.ctx())
                .unwrap()
                .session_id;
            let id2 = GenericProfile
                .extract_session_id(&c2.ctx())
                .unwrap()
                .session_id;
            assert_eq!(id1, "call_abc");
            assert_eq!(id1, id2, "canonicalized form must match across calls");
        }

        #[test]
        fn extract_session_id_none_for_text_only_history() {
            let req = r#"{"messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello"},
                {"role":"user","content":"more"}
            ]}"#;
            let c = oai(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_none_for_call_1_text_response() {
            let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
            let c = oai(Some(req), Some(resp));
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        /// Helper-shape one-shots have no tool-call anchor. They should stay
        /// visible as LLM calls but must not become synthetic generic turns.
        #[test]
        fn extract_session_id_none_for_helper_oneshots() {
            let sys = "You are a Claude agent built on Anthropic's Claude Agent SDK. \
                       Your task is to process Bash commands.";
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":"allow"}}]}"#;
            let make = |user: &str, ts_us: i64| {
                let req = serde_json::json!({
                    "messages": [
                        {"role": "system", "content": sys},
                        {"role": "user", "content": user},
                    ],
                })
                .to_string();
                let mut tc = oai(Some(&req), Some(resp));
                tc.call.request_time = ts_us;
                tc
            };
            // 5 calls within a single 60-second bucket (start aligned to a
            // bucket boundary so a `<60s` spread cannot straddle the next one).
            let bucket_start: i64 = 60_000_000 * 1_000;
            let calls: Vec<TestCall> = vec![
                make("Command: cat file_1", bucket_start + 0),
                make("Command: cat file_2", bucket_start + 5_000_000),
                make("Command: ls /workdir", bucket_start + 10_000_000),
                make("Command: rm temp.log", bucket_start + 30_000_000),
                make("Command: grep error log", bucket_start + 55_000_000),
            ];
            for c in &calls {
                assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
            }
        }

        #[test]
        fn extract_session_id_none_for_plain_openai_python_call() {
            let sys = "You are a Claude agent. Your task is to process Bash commands.";
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":"allow"}}]}"#;
            let req = serde_json::json!({
                "messages": [
                    {"role": "system", "content": sys},
                    {"role": "user", "content": "Choose one candidate."},
                ],
                "model": "qwen3.5-35b"
            })
            .to_string();
            let c = oai(Some(&req), Some(resp));
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
            let c = oai(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = oai(Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn is_user_turn_start_text() {
            let req = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&oai(Some(req), None).ctx()),
                Some(true),
            );
        }

        #[test]
        fn is_user_turn_start_false_when_last_is_tool() {
            let req = r#"{"messages":[
                {"role":"user","content":"x"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_a","content":"ok"}
            ]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&oai(Some(req), None).ctx()),
                Some(false),
            );
        }
    }

    // ─────────────── OpenAI Responses (wire_api=openai-responses) ──────────
    mod openai_responses_wire {
        use super::*;

        fn resp(req: Option<&str>, resp: Option<&str>) -> TestCall {
            call_with(wa::OPENAI_RESPONSES, vec![], req, resp)
        }

        #[test]
        fn extract_session_id_call_n_with_function_call() {
            let req = r#"{"input":[
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"sys"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"reasoning","summary":[],"content":[]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_xyz"}
            ]}"#;
            let c = resp(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert_eq!(
                ids.session_id, "fc_xyz",
                "function_call.call_id wins over assistant text"
            );
        }

        #[test]
        fn extract_session_id_call_1_function_call_in_response() {
            let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
            let r = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_abc"}]}"#;
            let c = resp(Some(req), Some(r));
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert_eq!(ids.session_id, "fc_abc");
        }

        #[test]
        fn extract_session_id_call_1_and_n_match() {
            let req1 = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]}]}"#;
            let r = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"}]}"#;
            let req2 = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"},
                {"type":"function_call_output","call_id":"fc_same","output":"ok"}
            ]}"#;
            let c1 = resp(Some(req1), Some(r));
            let c2 = resp(Some(req2), None);
            assert_eq!(
                GenericProfile
                    .extract_session_id(&c1.ctx())
                    .unwrap()
                    .session_id,
                GenericProfile
                    .extract_session_id(&c2.ctx())
                    .unwrap()
                    .session_id,
            );
        }

        #[test]
        fn extract_session_id_none_for_text_only_assistant() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}
            ]}"#;
            let c = resp(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_input_string_mode_treats_as_call_1() {
            let req = r#"{"input":"just a prompt"}"#;
            let r = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_simple"}]}"#;
            let c = resp(Some(req), Some(r));
            assert_eq!(
                GenericProfile
                    .extract_session_id(&c.ctx())
                    .unwrap()
                    .session_id,
                "fc_simple",
            );
        }

        #[test]
        fn extract_session_id_none_when_string_input_and_no_response() {
            let req = r#"{"input":"just a prompt"}"#;
            let c = resp(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
            let c = resp(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = resp(Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c.ctx()).is_none());
        }

        #[test]
        fn is_turn_terminal_delegates_to_helper() {
            let r = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}"#;
            let mut c = resp(None, Some(r));
            let wires = crate::wire_apis::build_default_wire_api_registry();
            assert!(GenericProfile.is_turn_terminal(&c.ctx(), &wires));
            c.set_response_body(
                r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#,
            );
            assert!(!GenericProfile.is_turn_terminal(&c.ctx(), &wires));
        }

        #[test]
        fn is_user_turn_start_last_user_message() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
                {"type":"function_call_output","call_id":"fc_a","output":"ok"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"more"}]}
            ]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&resp(Some(req), None).ctx()),
                Some(true),
            );
        }

        #[test]
        fn is_user_turn_start_false_when_last_is_function_call_output() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
                {"type":"function_call_output","call_id":"fc_a","output":"ok"}
            ]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&resp(Some(req), None).ctx()),
                Some(false),
            );
        }

        #[test]
        fn extract_assistant_text_returns_first_message_text() {
            let r = r#"{"output":[
                {"type":"reasoning","summary":[],"content":[]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"the answer"}]},
                {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}
            ]}"#;
            let c = resp(None, Some(r));
            assert_eq!(
                GenericProfile.extract_assistant_text(&c.ctx()).as_deref(),
                Some("the answer"),
            );
        }

        #[test]
        fn extract_assistant_text_none_when_no_message_item() {
            let r = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#;
            let c = resp(None, Some(r));
            assert_eq!(GenericProfile.extract_assistant_text(&c.ctx()), None);
        }
    }
}
