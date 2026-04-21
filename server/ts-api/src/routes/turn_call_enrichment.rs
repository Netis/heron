use serde::Serialize;
use ts_llm::model::{ParsedInput, ParsedOutput, WireApi};
use ts_llm::wire_api_registry::WireApiRegistry;
use ts_storage::query::TurnCallItem;

const ARGS_PREVIEW_LEN: usize = 200;
const REASONING_PREVIEW_LEN: usize = 120;
const MESSAGE_PREVIEW_LEN: usize = 60;

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedTurnCallItem {
    // Existing fields (flattened from TurnCallItem).
    pub id: String,
    pub sequence: u32,
    pub request_time: i64,
    pub response_time: Option<i64>,
    pub complete_time: Option<i64>,
    pub wire_api: String,
    pub model: String,
    pub status_code: Option<u16>,
    pub is_stream: bool,
    pub finish_reason: Option<String>,
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,

    // New Phase 2 fields.
    pub r#type: &'static str, // "tool_call" | "text" | "final"
    pub tool_calls: Vec<EnrichedToolCall>,
    pub has_reasoning: bool,
    pub reasoning_preview: Option<String>,
    pub message_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedToolCall {
    pub id: String,
    pub name: String,
    pub args_preview: String,
    pub result_summary: Option<ResultSummary>, // populated in Phase 3
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultSummary {
    pub size_bytes: u64,
    pub kind: &'static str,
    pub is_error: bool,
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>()
    }
}

pub fn enrich(
    items: Vec<TurnCallItem>,
    final_call_id: Option<&str>,
    registry: &WireApiRegistry,
) -> Vec<EnrichedTurnCallItem> {
    items
        .into_iter()
        .map(|c| {
            let wire = registry.find_by_name(&c.wire_api);
            let (parsed_out, _parsed_in) = wire
                .map(|w| parse_bodies(w, c.request_body.as_deref(), c.response_body.as_deref()))
                .unwrap_or((ParsedOutput::default(), ParsedInput::default()));

            let is_final = final_call_id.map(|f| f == c.id).unwrap_or(false);
            let type_str: &'static str = if is_final {
                "final"
            } else if !parsed_out.tool_calls.is_empty() {
                "tool_call"
            } else {
                "text"
            };

            let tool_calls = parsed_out
                .tool_calls
                .into_iter()
                .map(|tc| EnrichedToolCall {
                    id: tc.id,
                    name: tc.name,
                    args_preview: truncate(&tc.args_json, ARGS_PREVIEW_LEN),
                    result_summary: None,
                })
                .collect();

            EnrichedTurnCallItem {
                id: c.id,
                sequence: c.sequence,
                request_time: c.request_time,
                response_time: c.response_time,
                complete_time: c.complete_time,
                wire_api: c.wire_api,
                model: c.model,
                status_code: c.status_code,
                is_stream: c.is_stream,
                finish_reason: c.finish_reason,
                ttfb_ms: c.ttfb_ms,
                e2e_latency_ms: c.e2e_latency_ms,
                input_tokens: c.input_tokens,
                output_tokens: c.output_tokens,

                r#type: type_str,
                tool_calls,
                has_reasoning: parsed_out.reasoning.is_some(),
                reasoning_preview: parsed_out
                    .reasoning
                    .map(|s| truncate(&s, REASONING_PREVIEW_LEN)),
                message_preview: parsed_out
                    .message
                    .map(|s| truncate(&s, MESSAGE_PREVIEW_LEN)),
            }
        })
        .collect()
}

fn parse_bodies(
    wire: &dyn WireApi,
    req_body: Option<&str>,
    resp_body: Option<&str>,
) -> (ParsedOutput, ParsedInput) {
    let resp_val = resp_body
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let req_val = req_body
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    (wire.parse_output(&resp_val), wire.parse_input(&req_val))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts_llm::wire_apis::{build_default_wire_api_registry, ANTHROPIC};
    use ts_storage::query::TurnCallItem;

    fn anthropic_tool_use_body() -> String {
        r#"{"content":[{"type":"text","text":"let me check"},{"type":"tool_use","id":"toolu_abc","name":"read_file","input":{"path":"x"}}],"stop_reason":"tool_use"}"#.to_string()
    }

    fn mk_item(id: &str, wire: &str, body: &str) -> TurnCallItem {
        TurnCallItem {
            id: id.into(),
            sequence: 1,
            request_time: 0,
            response_time: None,
            complete_time: None,
            wire_api: wire.into(),
            model: "claude".into(),
            status_code: Some(200),
            is_stream: false,
            finish_reason: Some("tool_use".into()),
            ttfb_ms: None,
            e2e_latency_ms: Some(1000.0),
            input_tokens: None,
            output_tokens: None,
            request_body: None,
            response_body: Some(body.into()),
        }
    }

    #[test]
    fn enrich_marks_tool_call_type() {
        let reg = build_default_wire_api_registry();
        let items = vec![mk_item("c1", ANTHROPIC, &anthropic_tool_use_body())];
        let enriched = enrich(items, None, &reg);
        assert_eq!(enriched[0].r#type, "tool_call");
        assert_eq!(enriched[0].tool_calls.len(), 1);
        assert_eq!(enriched[0].tool_calls[0].name, "read_file");
    }

    #[test]
    fn enrich_marks_final_by_id() {
        let reg = build_default_wire_api_registry();
        let items = vec![mk_item("c1", ANTHROPIC, &anthropic_tool_use_body())];
        let enriched = enrich(items, Some("c1"), &reg);
        assert_eq!(enriched[0].r#type, "final");
    }
}
