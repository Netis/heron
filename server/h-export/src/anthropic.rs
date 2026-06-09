//! `anthropic` (Claude Messages API) → OpenAI-style SFT.
//!
//! Shape differences handled here:
//! - `system` (string | `[{type:text,text}]`) → a leading `system` message.
//! - assistant content blocks: `text` → content, `thinking` → `reasoning_content`, `tool_use` →
//!   `tool_calls` (its `input` is already a dict).
//! - a user message's `tool_result` blocks → one `role:"tool"` message **each** (Anthropic packs
//!   results into a user turn; OpenAI puts them in tool turns).
//! - tools `{name,description,input_schema}` →
//!   `{type:function,function:{name,description,parameters}}`.
//!
//! The response body is the reassembled assistant message and is appended as the
//! final assistant turn.

use serde_json::{json, Map, Value};

use crate::{
    arguments_to_dict, ExportError, Message, ToolCall, ToolCallFunction, TrainingTrajectory,
};

pub(crate) fn reconstruct(req: &Value, resp: &Value) -> Result<TrainingTrajectory, ExportError> {
    let mut messages: Vec<Message> = Vec::new();

    if let Some(text) = req.get("system").and_then(system_to_string) {
        if !text.is_empty() {
            messages.push(Message {
                role: "system".to_string(),
                content: Some(text),
                ..Default::default()
            });
        }
    }

    let req_msgs = req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ExportError::Malformed("anthropic", "request has no messages[]".into()))?;
    for m in req_msgs {
        convert_request_message_into(m, &mut messages);
    }

    // The response is the reassembled final assistant message.
    if let Some(asst) = assistant_from_content(resp.get("content"), resp.get("role")) {
        messages.push(asst);
    }

    let tools = req
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(convert_tool_schema).collect::<Vec<_>>())
        .unwrap_or_default();

    Ok(TrainingTrajectory {
        messages,
        tools,
        meta: json!({}),
    })
}

/// `system` is a string or an array of `{type:"text", text}` blocks.
fn system_to_string(sys: &Value) -> Option<String> {
    match sys {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

fn convert_request_message_into(m: &Value, out: &mut Vec<Message>) {
    let role = m.get("role").and_then(Value::as_str).unwrap_or_default();
    match role {
        "assistant" => {
            if let Some(asst) = assistant_from_content(m.get("content"), m.get("role")) {
                out.push(asst);
            }
        }
        _ => {
            // user (or anything else): plain text → user message; tool_result
            // blocks → one tool message each. Tool messages are emitted in block
            // order, with any free-standing user text appended after.
            match m.get("content") {
                Some(Value::String(s)) => out.push(Message {
                    role: "user".to_string(),
                    content: Some(s.clone()),
                    ..Default::default()
                }),
                Some(Value::Array(blocks)) => {
                    let mut user_text = String::new();
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str) {
                            Some("tool_result") => out.push(Message {
                                role: "tool".to_string(),
                                content: Some(tool_result_content_to_string(b.get("content"))),
                                tool_call_id: b
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .map(String::from),
                                ..Default::default()
                            }),
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    user_text.push_str(t);
                                }
                            }
                            _ => {} // image / other blocks: not part of the text trajectory
                        }
                    }
                    if !user_text.is_empty() {
                        out.push(Message {
                            role: "user".to_string(),
                            content: Some(user_text),
                            ..Default::default()
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

/// Convert an assistant content value (blocks array or string) into one
/// assistant `Message`. Returns `None` if there's nothing to emit.
fn assistant_from_content(content: Option<&Value>, role: Option<&Value>) -> Option<Message> {
    let role = role
        .and_then(Value::as_str)
        .unwrap_or("assistant")
        .to_string();
    match content {
        Some(Value::String(s)) => (!s.is_empty()).then(|| Message {
            role,
            content: Some(s.clone()),
            ..Default::default()
        }),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            let mut thinking = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                            thinking.push_str(t);
                        }
                    }
                    Some("tool_use") => tool_calls.push(ToolCall {
                        id: b
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        kind: "function".to_string(),
                        function: ToolCallFunction {
                            name: b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            arguments: arguments_to_dict(b.get("input")),
                        },
                    }),
                    _ => {}
                }
            }
            if text.is_empty() && thinking.is_empty() && tool_calls.is_empty() {
                return None;
            }
            Some(Message {
                role,
                content: (!text.is_empty()).then_some(text),
                reasoning_content: (!thinking.is_empty()).then_some(thinking),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                ..Default::default()
            })
        }
        _ => None,
    }
}

/// A `tool_result.content` is a string or an array of `{type:text,text}` blocks.
fn tool_result_content_to_string(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str).or_else(|| b.as_str()))
            .collect(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn convert_tool_schema(t: &Value) -> Value {
    let mut func = Map::new();
    func.insert(
        "name".to_string(),
        t.get("name").cloned().unwrap_or_else(|| json!("")),
    );
    if let Some(desc) = t.get("description") {
        func.insert("description".to_string(), desc.clone());
    }
    func.insert(
        "parameters".to_string(),
        t.get("input_schema").cloned().unwrap_or_else(|| json!({})),
    );
    json!({ "type": "function", "function": Value::Object(func) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_turn_with_thinking_tooluse_and_toolresult() {
        let req = json!({
            "system": [{"type": "text", "text": "diagnose pcaps"}],
            "messages": [
                {"role": "user", "content": "what broke?"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "let me look"},
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "tu_1", "name": "tshark", "input": {"filter": "tcp"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "100 packets"}
                ]}
            ],
            "tools": [
                {"name": "tshark", "description": "run tshark",
                 "input_schema": {"type": "object", "properties": {"filter": {"type": "string"}}}}
            ]
        });
        let resp = json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "RST flood from .5"}],
            "stop_reason": "end_turn"
        });

        let t = reconstruct(&req, &resp).unwrap();
        // system, user, assistant(think+text+toolcall), tool, final assistant
        assert_eq!(t.messages.len(), 5);

        assert_eq!(t.messages[0].role, "system");
        assert_eq!(t.messages[0].content.as_deref(), Some("diagnose pcaps"));

        assert_eq!(t.messages[1].role, "user");

        let asst = &t.messages[2];
        assert_eq!(asst.role, "assistant");
        assert_eq!(asst.content.as_deref(), Some("checking"));
        assert_eq!(asst.reasoning_content.as_deref(), Some("let me look"));
        let tc = &asst.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.id, "tu_1");
        assert_eq!(tc.function.name, "tshark");
        assert!(tc.function.arguments.is_object());
        assert_eq!(tc.function.arguments["filter"], json!("tcp"));

        // tool_result → role:"tool" with matching id
        let tool = &t.messages[3];
        assert_eq!(tool.role, "tool");
        assert_eq!(tool.tool_call_id.as_deref(), Some("tu_1"));
        assert_eq!(tool.content.as_deref(), Some("100 packets"));

        // final assistant from response
        assert_eq!(t.messages[4].role, "assistant");
        assert_eq!(t.messages[4].content.as_deref(), Some("RST flood from .5"));

        // tool schema converted: input_schema → parameters
        assert_eq!(t.tools.len(), 1);
        let f = &t.tools[0]["function"];
        assert_eq!(f["name"], json!("tshark"));
        assert_eq!(f["description"], json!("run tshark"));
        assert!(f["parameters"]["properties"]["filter"].is_object());
    }

    #[test]
    fn string_system_and_string_user() {
        let req = json!({
            "system": "be terse",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let resp = json!({"content": [{"type": "text", "text": "hello"}]});
        let t = reconstruct(&req, &resp).unwrap();
        assert_eq!(t.messages[0].content.as_deref(), Some("be terse"));
        assert_eq!(t.messages[1].content.as_deref(), Some("hi"));
        assert_eq!(t.messages[2].content.as_deref(), Some("hello"));
    }
}
