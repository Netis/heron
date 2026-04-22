use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_apis as wa;
use serde_json::Value;

pub struct CodexCliProfile;

const TURN_META_HEADER: &str = "x-codex-turn-metadata";
const CLIENT_REQ_ID_HEADER: &str = "x-client-request-id";
const SUBAGENT_HEADER: &str = "x-openai-subagent";
const ORIGINATOR_HEADER: &str = "originator";
const UA_PREFIXES: &[&str] = &["codex_cli_rs/", "codex-tui/", "codex_exec/"];
const ORIGINATOR_VALUES: &[&str] = &["codex_cli_rs", "codex-tui", "codex_exec"];

fn header<'a>(call: &'a LlmCall, key: &str) -> Option<&'a str> {
    call.request_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn parse_turn_metadata(raw: &str) -> Option<Value> {
    // Header may be base64-encoded per codex release notes; try raw JSON first, then base64.
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return Some(v);
    }
    // Attempt base64 decode — but avoid pulling in a dep; just skip if not raw JSON.
    // If codex rolls out b64 encoding widely we can add base64 crate later.
    None
}

impl AgentProfile for CodexCliProfile {
    fn name(&self) -> &'static str {
        "codex-cli"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        if call.wire_api != wa::OPENAI_RESPONSES {
            return false;
        }
        // Prefer Originator (stable short identifier); fall back to UA prefix.
        if let Some(orig) = header(call, ORIGINATOR_HEADER) {
            if ORIGINATOR_VALUES.contains(&orig) {
                return true;
            }
        }
        if let Some(ua) = header(call, "user-agent") {
            return UA_PREFIXES.iter().any(|p| ua.starts_with(p));
        }
        false
    }

    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
        // Preferred: parse X-Codex-Turn-Metadata JSON.
        if let Some(raw) = header(call, TURN_META_HEADER) {
            if let Some(v) = parse_turn_metadata(raw) {
                let session_id = v.get("session_id")?.as_str()?.to_string();
                let turn_id = v
                    .get("turn_id")
                    .and_then(|t| t.as_str())
                    .map(str::to_string);
                return Some(ExtractedIds {
                    session_id,
                    turn_id,
                });
            }
        }
        // Fallback: X-Client-Request-Id as session; no turn_id ⇒ tracker generates.
        let session_id = header(call, CLIENT_REQ_ID_HEADER)?.to_string();
        Some(ExtractedIds {
            session_id,
            turn_id: None,
        })
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        // Inspect input[-1]: message(role=user) ⇒ new turn;
        // function_call_output / reasoning / function_call ⇒ continuation.
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let inp = v.get("input")?.as_array()?;
        let last = inp.last()?;
        match last.get("type").and_then(|t| t.as_str())? {
            "message" => match last.get("role").and_then(|r| r.as_str()) {
                Some("user") => Some(true),
                _ => Some(false),
            },
            "function_call_output" | "reasoning" | "function_call" => Some(false),
            _ => None,
        }
    }

    fn subagent(&self, call: &LlmCall) -> Option<String> {
        header(call, SUBAGENT_HEADER).map(str::to_string)
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let inp = v.get("input")?.as_array()?;
        let last = inp.last()?;
        if last.get("type").and_then(|t| t.as_str()) != Some("message") {
            return None;
        }
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return None;
        }
        let text = match last.get("content")? {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter(|b| {
                        matches!(
                            b.get("type").and_then(|t| t.as_str()),
                            Some("input_text") | Some("text")
                        )
                    })
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
                    .collect();
                parts.join("\n")
            }
            _ => return None,
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn is_turn_terminal(&self, call: &LlmCall) -> bool {
        // Codex's per-call finish_reason maps `response.completed` → Complete
        // for every successful API call, so it cannot distinguish "agent done"
        // from "tool roundtrip pending". Inspect response.output instead:
        // any item whose type triggers a follow-up API call (function_call,
        // custom_tool_call, local_shell_call, MCP variants, etc.) means the
        // turn is not terminal. A response containing only message/reasoning
        // items is the agent's final answer.
        let Some(body) = call.response_body.as_deref() else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<Value>(body) else {
            return false;
        };
        let Some(output) = v.get("output").and_then(|o| o.as_array()) else {
            return false;
        };
        let mut has_message = false;
        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => has_message = true,
                Some("reasoning") => {}
                // Any *_call item means codex will execute a tool and POST
                // back another request — turn is not terminal.
                Some(t) if t.ends_with("_call") => return false,
                _ => {}
            }
        }
        has_message
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        // Responses API: { output: [ { type: "message", content: [ { type: "output_text", text } ] } ] }
        let mut parts: Vec<String> = Vec::new();
        if let Some(output) = v.get("output").and_then(|o| o.as_array()) {
            for item in output {
                if item.get("type").and_then(|t| t.as_str()) != Some("message") {
                    continue;
                }
                if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                    for block in content {
                        if matches!(
                            block.get("type").and_then(|t| t.as_str()),
                            Some("output_text") | Some("text")
                        ) {
                            if let Some(txt) = block.get("text").and_then(|t| t.as_str()) {
                                parts.push(txt.to_string());
                            }
                        }
                    }
                }
            }
        }
        // Chat Completions fallback: { choices: [ { message: { content } } ] }
        if parts.is_empty() {
            if let Some(content) = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                parts.push(content.to_string());
            }
        }
        if parts.is_empty() {
            return None;
        }
        let joined = parts.join("\n");
        if joined.trim().is_empty() {
            None
        } else {
            Some(joined)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(
        wire_api: &'static str,
        headers: Vec<(&str, &str)>,
        body: Option<&str>,
    ) -> LlmCall {
        LlmCall {
            stream_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "gpt".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/responses".into(),
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

    #[test]
    fn matches_by_originator() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("Originator", "codex_cli_rs")],
            None,
        );
        assert!(CodexCliProfile.matches(&c));
    }

    #[test]
    fn matches_codex_tui_by_ua() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "codex-tui/0.118.0 (Mac OS)")],
            None,
        );
        assert!(CodexCliProfile.matches(&c));
    }

    #[test]
    fn matches_codex_exec_by_originator() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("Originator", "codex_exec")],
            None,
        );
        assert!(CodexCliProfile.matches(&c));
    }

    #[test]
    fn matches_codex_exec_by_ua() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![("User-Agent", "codex_exec/0.120.0 (Ubuntu 24.4.0; x86_64)")],
            None,
        );
        assert!(CodexCliProfile.matches(&c));
    }

    #[test]
    fn does_not_match_chat_api() {
        let c = call_with(wa::OPENAI_CHAT, vec![("Originator", "codex_cli_rs")], None);
        assert!(!CodexCliProfile.matches(&c));
    }

    #[test]
    fn extract_ids_from_turn_metadata_header() {
        let meta = r#"{"session_id":"019d7170-77f6-7eb3-9c93-2e19cbdf9a86","turn_id":"019d7170-7806-7ff0-9d84-8c917b132acd","workspaces":{}}"#;
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![
                ("Originator", "codex_cli_rs"),
                ("X-Codex-Turn-Metadata", meta),
            ],
            None,
        );
        let ids = CodexCliProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "019d7170-77f6-7eb3-9c93-2e19cbdf9a86");
        assert_eq!(
            ids.turn_id.as_deref(),
            Some("019d7170-7806-7ff0-9d84-8c917b132acd")
        );
    }

    #[test]
    fn extract_ids_fallback_to_client_request_id() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![
                ("Originator", "codex_cli_rs"),
                ("X-Client-Request-Id", "abc-123"),
            ],
            None,
        );
        let ids = CodexCliProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "abc-123");
        assert!(ids.turn_id.is_none());
    }

    #[test]
    fn is_user_turn_start_message_role_user() {
        let body = r#"{"input":[{"type":"message","role":"user","content":"hi"}]}"#;
        let c = call_with(wa::OPENAI_RESPONSES, vec![], Some(body));
        assert_eq!(CodexCliProfile.is_user_turn_start(&c), Some(true));
    }

    #[test]
    fn is_user_turn_start_function_call_output() {
        let body = r#"{"input":[{"type":"function_call_output","call_id":"c1","output":"{}"}]}"#;
        let c = call_with(wa::OPENAI_RESPONSES, vec![], Some(body));
        assert_eq!(CodexCliProfile.is_user_turn_start(&c), Some(false));
    }

    #[test]
    fn is_user_turn_start_reasoning_is_continuation() {
        let body = r#"{"input":[{"type":"reasoning","content":"..."}]}"#;
        let c = call_with(wa::OPENAI_RESPONSES, vec![], Some(body));
        assert_eq!(CodexCliProfile.is_user_turn_start(&c), Some(false));
    }

    #[test]
    fn extract_user_input_from_input_text_blocks() {
        let body = r#"{"input":[
            {"type":"function_call_output","call_id":"c1","output":"{}"},
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"please refactor X"}
            ]}
        ]}"#;
        let c = call_with(wa::OPENAI_RESPONSES, vec![], Some(body));
        assert_eq!(
            CodexCliProfile.extract_user_input(&c).as_deref(),
            Some("please refactor X")
        );
    }

    #[test]
    fn extract_user_input_string_content() {
        let body = r#"{"input":[{"type":"message","role":"user","content":"hi"}]}"#;
        let c = call_with(wa::OPENAI_RESPONSES, vec![], Some(body));
        assert_eq!(
            CodexCliProfile.extract_user_input(&c).as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn extract_user_input_none_when_last_is_tool_output() {
        let body = r#"{"input":[{"type":"function_call_output","call_id":"c1","output":"{}"}]}"#;
        let c = call_with(wa::OPENAI_RESPONSES, vec![], Some(body));
        assert_eq!(CodexCliProfile.extract_user_input(&c), None);
    }

    #[test]
    fn extract_assistant_text_from_responses_output() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[]},
            {"type":"message","role":"assistant","content":[
                {"type":"output_text","text":"done."}
            ]}
        ]}"#;
        let mut c = call_with(wa::OPENAI_RESPONSES, vec![], None);
        c.response_body = Some(body.to_string());
        assert_eq!(
            CodexCliProfile.extract_assistant_text(&c).as_deref(),
            Some("done.")
        );
    }

    #[test]
    fn extract_assistant_text_chat_completions_fallback() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello from chat"}}]}"#;
        let mut c = call_with(wa::OPENAI_RESPONSES, vec![], None);
        c.response_body = Some(body.to_string());
        assert_eq!(
            CodexCliProfile.extract_assistant_text(&c).as_deref(),
            Some("hello from chat")
        );
    }

    #[test]
    fn is_turn_terminal_true_when_output_only_has_message() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"done."}]}
        ]}"#;
        let mut c = call_with(wa::OPENAI_RESPONSES, vec![], None);
        c.response_body = Some(body.to_string());
        assert!(CodexCliProfile.is_turn_terminal(&c));
    }

    #[test]
    fn is_turn_terminal_false_when_output_has_function_call() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[]},
            {"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}
        ]}"#;
        let mut c = call_with(wa::OPENAI_RESPONSES, vec![], None);
        c.response_body = Some(body.to_string());
        assert!(!CodexCliProfile.is_turn_terminal(&c));
    }

    #[test]
    fn is_turn_terminal_false_when_output_has_message_and_function_call() {
        // Codex sometimes returns text alongside a tool call; the call still
        // forces another roundtrip.
        let body = r#"{"output":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"running"}]},
            {"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}
        ]}"#;
        let mut c = call_with(wa::OPENAI_RESPONSES, vec![], None);
        c.response_body = Some(body.to_string());
        assert!(!CodexCliProfile.is_turn_terminal(&c));
    }

    #[test]
    fn is_turn_terminal_false_when_output_only_reasoning() {
        // No final message → not a final answer.
        let body = r#"{"output":[{"type":"reasoning","summary":[]}]}"#;
        let mut c = call_with(wa::OPENAI_RESPONSES, vec![], None);
        c.response_body = Some(body.to_string());
        assert!(!CodexCliProfile.is_turn_terminal(&c));
    }

    #[test]
    fn is_turn_terminal_false_when_no_response_body() {
        let c = call_with(wa::OPENAI_RESPONSES, vec![], None);
        assert!(!CodexCliProfile.is_turn_terminal(&c));
    }

    #[test]
    fn subagent_header_returned() {
        let c = call_with(
            wa::OPENAI_RESPONSES,
            vec![
                ("Originator", "codex_cli_rs"),
                ("X-Openai-Subagent", "review"),
            ],
            None,
        );
        assert_eq!(CodexCliProfile.subagent(&c).as_deref(), Some("review"));
    }
}
