//! Generic agent profile — synthesizes a session id from the request /
//! response payload alone, so TokenScope produces `AgentTurn`s for header-less
//! LLM traffic across all three supported wire APIs (Anthropic Messages,
//! OpenAI Chat Completions, OpenAI Responses).
//!
//! `agent_kind == "generic"` is wire-api-agnostic; downstream consumers read
//! `wire_api` separately. Per-shape parsing lives in `wire_apis::{anthropic,
//! openai::chat, openai::responses}` and is shared with `openclaw`; the
//! profile is a thin dispatcher on `call.wire_api`.

use crate::profile::{AgentProfile, CallCtx, SessionIdExtraction};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;

use super::session_id::{compose_session_id_tracked, synth_helper_session_id};
use crate::wire_apis::AssistantSig;

pub struct GenericProfile;

impl AgentProfile for GenericProfile {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn matches(&self, ctx: &CallCtx<'_>) -> bool {
        matches!(
            ctx.call.wire_api,
            wa::ANTHROPIC | wa::OPENAI_CHAT | wa::OPENAI_RESPONSES
        )
    }

    fn extract_session_id(&self, ctx: &CallCtx<'_>) -> Option<SessionIdExtraction> {
        let req = ctx.req?;
        // Track three pieces per wire api:
        //   * user_text   — first user message (used for the conversation
        //                   text-hash path; required for everything below).
        //   * sig_in_req  — Some(sig) if an assistant message already lives
        //                   inside `req.messages[*]` / `req.input[*]`. This
        //                   tells us the call belongs to a multi-turn
        //                   conversation; the caller has already established
        //                   a stable anchor we can hash.
        //   * sig_in_resp — Some(sig) only when sig_in_req is None and the
        //                   response carries the assistant turn for the
        //                   first user message. Used both for text-hash
        //                   fallback and for routing helper-shape one-shots
        //                   to the system+time bucket.
        //   * system_text — first system message; the only signal a
        //                   helper-shape one-shot ever has that's stable
        //                   across replays of the same helper sub-agent.
        let (user_text, sig_in_req, sig_in_resp, system_text) = match ctx.call.wire_api {
            wa::ANTHROPIC => {
                let user_text = wa::anthropic::first_user_text(req)?;
                let sig_in_req = wa::anthropic::first_assistant_sig_from_request(req);
                let sig_in_resp = if sig_in_req.is_none() {
                    ctx.resp
                        .and_then(wa::anthropic::first_assistant_sig_from_response_value)
                } else {
                    None
                };
                let system_text = wa::anthropic::first_system_text(req);
                (user_text, sig_in_req, sig_in_resp, system_text)
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
                let system_text = wa::openai::chat::first_system_text(req);
                (user_text, sig_in_req, sig_in_resp, system_text)
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
                let system_text = wa::openai::responses::first_system_text(req);
                (user_text, sig_in_req, sig_in_resp, system_text)
            }
            _ => return None,
        };

        // Helper-shape one-shot detection: the request has no assistant turn
        // baked in (`sig_in_req` is None) AND the response side is plain
        // text rather than a tool call (a tool id would already be a
        // perfectly stable session anchor on its own). Hash the system
        // prompt + a coarse time bucket so all replays of the same helper
        // sub-agent within one agent run collapse into one session_id.
        if sig_in_req.is_none() {
            if let (Some(AssistantSig::Text(_)), Some(sys)) = (&sig_in_resp, &system_text) {
                if !sys.is_empty() {
                    let session_id = synth_helper_session_id(sys, ctx.call.request_time);
                    return Some(SessionIdExtraction {
                        session_id: format!("gen-{session_id}"),
                        tool_id_canonicalized: false,
                    });
                }
            }
        }

        let sig = sig_in_req.or(sig_in_resp)?;
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
        let path = match wire_api {
            wa::ANTHROPIC => "/v1/messages",
            wa::OPENAI_CHAT => "/v1/chat/completions",
            wa::OPENAI_RESPONSES => "/v1/responses",
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
            request_headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            response_headers: vec![],
        })
    }

    // ── Cross-wire-api: matches() ───────────────────────────────────────────

    #[test]
    fn matches_all_three_wire_apis() {
        for &wire in &[wa::ANTHROPIC, wa::OPENAI_CHAT, wa::OPENAI_RESPONSES] {
            let c = call_with(wire, vec![], None, None);
            assert!(GenericProfile.matches(&c.ctx()), "should match {wire}");
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
        fn extract_session_id_call_n_with_text_only_history() {
            let req = r#"{"messages":[
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[{"type":"text","text":"hello there"}]},
                {"role":"user","content":[{"type":"text","text":"more"}]}
            ]}"#;
            let c = ant(vec![], Some(req), None);
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
            assert_eq!(ids.session_id.len(), "gen-".len() + 16);
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
        fn extract_session_id_call_1_text_in_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let resp = r#"{"content":[{"type":"text","text":"hello there"}]}"#;
            let c = ant(vec![], Some(req), Some(resp));
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
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
        fn extract_session_id_call_n_with_text_only_history() {
            let req = r#"{"messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello"},
                {"role":"user","content":"more"}
            ]}"#;
            let c = oai(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_call_1_text_in_response() {
            let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
            let c = oai(Some(req), Some(resp));
            assert!(GenericProfile
                .extract_session_id(&c.ctx())
                .unwrap()
                .session_id
                .starts_with("gen-"));
        }

        /// Helper-shape one-shot calls (request = [system, user] only, response
        /// = assistant text) coming from a Claude-Agent-SDK helper sub-agent
        /// like the Bash permission gate or path-extractor: each call has the
        /// SAME system message but a DIFFERENT user message (different
        /// command being checked). Without a system+time fallback, every
        /// call gets a unique gen-<hash> id and the agent_turns view shows
        /// each helper as its own 1-call turn.
        ///
        /// With the fallback, all helpers within the time-bucket window share
        /// a session_id and group into a single agent_turn.
        #[test]
        fn extract_session_id_helper_oneshots_share_session_id_via_system_msg() {
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
            let ids: Vec<String> = calls
                .iter()
                .map(|c| {
                    GenericProfile
                        .extract_session_id(&c.ctx())
                        .unwrap()
                        .session_id
                })
                .collect();
            // All 5 must share the same session_id (they're the same helper
            // sub-agent firing repeatedly within one agent run).
            let first = &ids[0];
            for (i, id) in ids.iter().enumerate() {
                assert_eq!(
                    id, first,
                    "call {} session_id={} differs from first={}; all helpers must share id",
                    i, id, first
                );
            }
            // Sanity: still a `gen-` synthesised id (not a tool id leak).
            assert!(first.starts_with("gen-"));
        }

        /// Two helper batches separated by a long idle gap (> bucket window)
        /// must split into two distinct session_ids — otherwise running the
        /// same agent twice in a day collapses both runs' helpers into one
        /// turn.
        #[test]
        fn extract_session_id_helper_oneshots_split_across_idle_gap() {
            let sys = "You are a Claude agent. Your task is to process Bash commands.";
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
            let early = make("Command: ls", 1_000_000_000);
            let late = make("Command: ls", 1_000_000_000 + 5 * 60_000_000);
            let id1 = GenericProfile
                .extract_session_id(&early.ctx())
                .unwrap()
                .session_id;
            let id2 = GenericProfile
                .extract_session_id(&late.ctx())
                .unwrap()
                .session_id;
            assert_ne!(
                id1, id2,
                "calls 5 minutes apart on same helper must be different sessions"
            );
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
        fn extract_session_id_call_n_with_text_only_assistant() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}
            ]}"#;
            let c = resp(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c.ctx()).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
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
