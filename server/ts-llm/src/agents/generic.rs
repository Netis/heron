//! Generic agent profile — synthesizes a session id from the request /
//! response payload alone, so TokenScope produces `AgentTurn`s for header-less
//! LLM traffic across all three supported wire APIs (Anthropic Messages,
//! OpenAI Chat Completions, OpenAI Responses).
//!
//! `agent_kind == "generic"` is wire-api-agnostic; downstream consumers read
//! `wire_api` separately. Per-shape parsing lives in `wire_apis::{anthropic,
//! openai::chat, openai::responses}` and is shared with `openclaw`; the
//! profile is a thin dispatcher on `call.wire_api`.

use crate::model::LlmCall;
use crate::profile::{AgentProfile, SessionIdExtraction};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;

use super::session_id::compose_session_id_tracked;

pub struct GenericProfile;

impl AgentProfile for GenericProfile {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        matches!(
            call.wire_api,
            wa::ANTHROPIC | wa::OPENAI_CHAT | wa::OPENAI_RESPONSES
        )
    }

    fn extract_session_id(&self, call: &LlmCall) -> Option<SessionIdExtraction> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let (user_text, sig) = match call.wire_api {
            wa::ANTHROPIC => {
                let user_text = wa::anthropic::first_user_text(&v)?;
                let sig = wa::anthropic::first_assistant_sig_from_request(&v).or_else(|| {
                    call.response_body
                        .as_deref()
                        .and_then(wa::anthropic::first_assistant_sig_from_response)
                })?;
                (user_text, sig)
            }
            wa::OPENAI_CHAT => {
                let user_text = wa::openai::chat::first_user_text(&v)?;
                let sig = wa::openai::chat::first_assistant_sig_from_request(&v).or_else(|| {
                    call.response_body
                        .as_deref()
                        .and_then(wa::openai::chat::first_assistant_sig_from_response)
                })?;
                (user_text, sig)
            }
            wa::OPENAI_RESPONSES => {
                // Responses works on the `input` array (or string-shorthand)
                // rather than `messages`, so it has its own walker layer.
                let (user_text, sig_from_input) = match v.get("input")? {
                    serde_json::Value::Array(items) => (
                        wa::openai::responses::first_user_text(items),
                        wa::openai::responses::first_assistant_sig_from_input(items),
                    ),
                    serde_json::Value::String(s) if !s.trim().is_empty() => (Some(s.clone()), None),
                    _ => (None, None),
                };
                let user_text = user_text?;
                let sig = sig_from_input.or_else(|| {
                    call.response_body
                        .as_deref()
                        .and_then(wa::openai::responses::first_assistant_sig_from_response)
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

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        match call.wire_api {
            wa::ANTHROPIC => wa::anthropic::is_user_turn_start(&v),
            wa::OPENAI_CHAT => wa::openai::chat::is_user_turn_start(&v),
            wa::OPENAI_RESPONSES => wa::openai::responses::is_user_turn_start(&v),
            _ => None,
        }
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        match call.wire_api {
            wa::ANTHROPIC => wa::anthropic::extract_user_input(&v),
            wa::OPENAI_CHAT => wa::openai::chat::extract_user_input(&v),
            wa::OPENAI_RESPONSES => wa::openai::responses::extract_user_input(&v),
            _ => None,
        }
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        match call.wire_api {
            wa::ANTHROPIC => wa::anthropic::extract_assistant_text(body),
            wa::OPENAI_CHAT => wa::openai::chat::extract_assistant_text(body),
            wa::OPENAI_RESPONSES => wa::openai::responses::extract_assistant_text(body),
            _ => None,
        }
    }

    fn is_turn_terminal(&self, call: &LlmCall, wire_apis: &WireApiRegistry) -> bool {
        // OpenAI Responses' wire-api `status: "completed"` is unreliable
        // (always present even on tool-roundtrip pending), so inspect the
        // response body directly — same reasoning as `CodexCliProfile`.
        // Anthropic and OpenAI Chat fall through to the trait-default
        // implicit-path dispatch (duplicated here because traits have no
        // `super` to call).
        if call.wire_api == wa::OPENAI_RESPONSES {
            crate::wire_apis::openai::body_has_terminal_message_only(call.response_body.as_deref())
        } else {
            let Some(reason) = call.finish_reason.as_deref() else {
                return false;
            };
            let Some(api) = wire_apis.find_by_name(call.wire_api) else {
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
    use std::net::IpAddr;

    fn call_with(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        req: Option<&str>,
        resp: Option<&str>,
    ) -> LlmCall {
        let path = match wire_api {
            wa::ANTHROPIC => "/v1/messages",
            wa::OPENAI_CHAT => "/v1/chat/completions",
            wa::OPENAI_RESPONSES => "/v1/responses",
            _ => "/",
        };
        LlmCall {
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
        }
    }

    // ── Cross-wire-api: matches() ───────────────────────────────────────────

    #[test]
    fn matches_all_three_wire_apis() {
        for &wire in &[wa::ANTHROPIC, wa::OPENAI_CHAT, wa::OPENAI_RESPONSES] {
            let c = call_with(wire, vec![], None, None);
            assert!(GenericProfile.matches(&c), "should match {wire}");
        }
    }

    // ───────────────────── Anthropic (wire_api=anthropic) ──────────────────
    mod anthropic_wire {
        use super::*;

        fn ant(headers: Vec<(&str, &str)>, req: Option<&str>, resp: Option<&str>) -> LlmCall {
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
            let ids = GenericProfile.extract_session_id(&c).unwrap();
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
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
            assert_eq!(ids.session_id.len(), "gen-".len() + 16);
        }

        #[test]
        fn extract_session_id_call_1_tool_in_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let resp =
                r#"{"content":[{"type":"tool_use","id":"toolu_xyz","name":"Read","input":{}}]}"#;
            let c = ant(vec![], Some(req), Some(resp));
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert_eq!(ids.session_id, "toolu_xyz");
        }

        #[test]
        fn extract_session_id_call_1_text_in_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let resp = r#"{"content":[{"type":"text","text":"hello there"}]}"#;
            let c = ant(vec![], Some(req), Some(resp));
            let ids = GenericProfile.extract_session_id(&c).unwrap();
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
            let id1 = GenericProfile.extract_session_id(&c1).unwrap().session_id;
            let id2 = GenericProfile.extract_session_id(&c2).unwrap().session_id;
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
                GenericProfile.extract_session_id(&c1).unwrap().session_id,
                GenericProfile.extract_session_id(&c2).unwrap().session_id,
            );
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = ant(vec![], Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn is_user_turn_start_text() {
            let req =
                r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert_eq!(GenericProfile.is_user_turn_start(&c), Some(true));
        }

        #[test]
        fn is_user_turn_start_tool_result_only() {
            let req = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
            let c = ant(vec![], Some(req), None);
            assert_eq!(GenericProfile.is_user_turn_start(&c), Some(false));
        }
    }

    // ─────────────────── OpenAI Chat (wire_api=openai-chat) ────────────────
    mod openai_chat_wire {
        use super::*;

        fn oai(req: Option<&str>, resp: Option<&str>) -> LlmCall {
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
            let ids = GenericProfile.extract_session_id(&c).unwrap();
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
            let id1 = GenericProfile.extract_session_id(&c1).unwrap().session_id;
            let id2 = GenericProfile.extract_session_id(&c2).unwrap().session_id;
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
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_call_1_text_in_response() {
            let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
            let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
            let c = oai(Some(req), Some(resp));
            assert!(GenericProfile
                .extract_session_id(&c)
                .unwrap()
                .session_id
                .starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
            let c = oai(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = oai(Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn is_user_turn_start_text() {
            let req = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
            assert_eq!(
                GenericProfile.is_user_turn_start(&oai(Some(req), None)),
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
                GenericProfile.is_user_turn_start(&oai(Some(req), None)),
                Some(false),
            );
        }
    }

    // ─────────────── OpenAI Responses (wire_api=openai-responses) ──────────
    mod openai_responses_wire {
        use super::*;

        fn resp(req: Option<&str>, resp: Option<&str>) -> LlmCall {
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
            let ids = GenericProfile.extract_session_id(&c).unwrap();
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
            let ids = GenericProfile.extract_session_id(&c).unwrap();
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
                GenericProfile.extract_session_id(&c1).unwrap().session_id,
                GenericProfile.extract_session_id(&c2).unwrap().session_id,
            );
        }

        #[test]
        fn extract_session_id_call_n_with_text_only_assistant() {
            let req = r#"{"input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}
            ]}"#;
            let c = resp(Some(req), None);
            let ids = GenericProfile.extract_session_id(&c).unwrap();
            assert!(ids.session_id.starts_with("gen-"));
        }

        #[test]
        fn extract_session_id_input_string_mode_treats_as_call_1() {
            let req = r#"{"input":"just a prompt"}"#;
            let r = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_simple"}]}"#;
            let c = resp(Some(req), Some(r));
            assert_eq!(
                GenericProfile.extract_session_id(&c).unwrap().session_id,
                "fc_simple",
            );
        }

        #[test]
        fn extract_session_id_none_when_string_input_and_no_response() {
            let req = r#"{"input":"just a prompt"}"#;
            let c = resp(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_first_call_no_response() {
            let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
            let c = resp(Some(req), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn extract_session_id_none_when_malformed_json() {
            let c = resp(Some("garbage"), None);
            assert!(GenericProfile.extract_session_id(&c).is_none());
        }

        #[test]
        fn is_turn_terminal_delegates_to_helper() {
            let r = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}"#;
            let mut c = resp(None, Some(r));
            let wires = crate::wire_apis::build_default_wire_api_registry();
            assert!(GenericProfile.is_turn_terminal(&c, &wires));
            c.response_body = Some(
                r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#
                    .to_string(),
            );
            assert!(!GenericProfile.is_turn_terminal(&c, &wires));
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
                GenericProfile.is_user_turn_start(&resp(Some(req), None)),
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
                GenericProfile.is_user_turn_start(&resp(Some(req), None)),
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
                GenericProfile.extract_assistant_text(&c).as_deref(),
                Some("the answer"),
            );
        }

        #[test]
        fn extract_assistant_text_none_when_no_message_item() {
            let r = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#;
            let c = resp(None, Some(r));
            assert_eq!(GenericProfile.extract_assistant_text(&c), None);
        }
    }
}
