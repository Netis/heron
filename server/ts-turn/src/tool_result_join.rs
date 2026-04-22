//! Join tool_use blocks from call N to tool_result blocks from call N+1.
//!
//! The backend parses each call's request/response body into `ParsedInput`
//! and `ParsedOutput` (see ts-llm::parse). Tool-use blocks are emitted by
//! call N's output; their corresponding tool_results live in call N+1's
//! input (indexed by tool_use_id). This module walks adjacent pairs and
//! attaches each result to its call.

use ts_llm::model::{ParsedInput, ParsedOutput, ParsedToolResult};

/// Walk adjacent call pairs and attach each tool_use's result (if any) from
/// the successor call's input. A call that has no successor leaves results as
/// `None` — the UI renders "(no response, turn ended)".
pub fn attach_tool_results<'a>(
    outputs: &'a [ParsedOutput],
    inputs: &'a [ParsedInput],
) -> Vec<Vec<(String /*tool_use_id*/, Option<&'a ParsedToolResult>)>> {
    assert_eq!(
        outputs.len(),
        inputs.len(),
        "outputs and inputs must be 1-1 and sorted by call sequence"
    );
    outputs
        .iter()
        .enumerate()
        .map(|(i, out)| {
            let next_input = inputs.get(i + 1);
            out.tool_calls
                .iter()
                .map(|tc| {
                    let r = next_input
                        .and_then(|ni| ni.tool_results.iter().find(|tr| tr.tool_use_id == tc.id));
                    (tc.id.clone(), r)
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts_llm::model::{ParsedInput, ParsedOutput, ParsedToolCall, ParsedToolResult};

    fn out_with_tc(id: &str) -> ParsedOutput {
        ParsedOutput {
            reasoning: None,
            message: None,
            tool_calls: vec![ParsedToolCall {
                id: id.into(),
                name: "x".into(),
                args_json: "{}".into(),
            }],
        }
    }

    fn input_with_tr(id: &str) -> ParsedInput {
        ParsedInput {
            tool_results: vec![ParsedToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn attaches_result_from_next_call() {
        let outs = vec![out_with_tc("tc1"), ParsedOutput::default()];
        let ins = vec![ParsedInput::default(), input_with_tr("tc1")];
        let joined = attach_tool_results(&outs, &ins);
        assert_eq!(joined[0].len(), 1);
        assert_eq!(joined[0][0].0, "tc1");
        assert!(joined[0][0].1.is_some());
    }

    #[test]
    fn last_call_tool_use_has_no_result() {
        let outs = vec![out_with_tc("tc_orphan")];
        let ins = vec![ParsedInput::default()];
        let joined = attach_tool_results(&outs, &ins);
        assert_eq!(joined[0][0].1.map(|_| ()), None);
    }

    #[test]
    fn mismatched_id_returns_none() {
        let outs = vec![out_with_tc("tc1"), ParsedOutput::default()];
        let ins = vec![ParsedInput::default(), input_with_tr("other")];
        let joined = attach_tool_results(&outs, &ins);
        assert!(joined[0][0].1.is_none());
    }
}
