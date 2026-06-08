//! `openai-chat` → OpenAI-style SFT. The request is already OpenAI-shaped, so this
//! is mostly pass-through; the one transform is parsing each assistant
//! `tool_calls[].function.arguments` from its JSON-string form into a dict.

use serde_json::{json, Value};

use crate::{
    arguments_to_dict, ExportError, Message, ToolCall, ToolCallFunction, TrainingTrajectory,
};

pub(crate) fn reconstruct(req: &Value, resp: &Value) -> Result<TrainingTrajectory, ExportError> {
    let req_msgs = req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ExportError::Malformed("openai-chat", "request has no messages[]".into()))?;

    let mut messages: Vec<Message> = req_msgs.iter().map(convert_message).collect();

    // Append the final assistant turn from the response.
    if let Some(msg) = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
    {
        messages.push(convert_message(msg));
    }

    let tools = req
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    Ok(TrainingTrajectory {
        messages,
        tools,
        meta: json!({}),
    })
}

fn convert_message(m: &Value) -> Message {
    let role = m
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let reasoning_content = m
        .get("reasoning_content")
        .or_else(|| m.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let tool_calls = m
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(convert_tool_call).collect::<Vec<_>>())
        .filter(|v| !v.is_empty());

    Message {
        role,
        content: content_to_string(m.get("content")),
        reasoning_content,
        tool_calls,
        tool_call_id: m
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(String::from),
        name: m.get("name").and_then(Value::as_str).map(String::from),
    }
}

fn convert_tool_call(tc: &Value) -> Option<ToolCall> {
    let function = tc.get("function")?;
    Some(ToolCall {
        id: tc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind: "function".to_string(),
        function: ToolCallFunction {
            name: function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: arguments_to_dict(function.get("arguments")),
        },
    })
}

/// OpenAI `content` is a string or an array of parts (`{type:"text",text}`).
/// Join text parts; `None` when absent/empty (e.g. an assistant turn that is
/// only `tool_calls`).
fn content_to_string(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let joined: String = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str()))
                .collect();
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn arguments_string_to_dict_and_reasoning_mapped() {
        let req = json!({
            "messages": [
                {"role": "system", "content": "be helpful"},
                {"role": "user", "content": "trace this"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "name": "shell", "content": "a.txt"}
            ],
            "tools": [{"type": "function", "function": {"name": "shell", "parameters": {}}}]
        });
        let resp = json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": "done",
                "reasoning": "I listed the dir"
            }}]
        });

        let t = reconstruct(&req, &resp).unwrap();
        assert_eq!(t.messages.len(), 5);

        // assistant tool call: arguments parsed to a dict
        let asst = &t.messages[2];
        let tc = &asst.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.id, "call_1");
        assert!(tc.function.arguments.is_object());
        assert_eq!(tc.function.arguments["cmd"], json!("ls"));

        // tool result message
        let tool = &t.messages[3];
        assert_eq!(tool.role, "tool");
        assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));

        // final assistant from response, bare `reasoning` → reasoning_content
        let final_asst = t.messages.last().unwrap();
        assert_eq!(final_asst.role, "assistant");
        assert_eq!(final_asst.content.as_deref(), Some("done"));
        assert_eq!(
            final_asst.reasoning_content.as_deref(),
            Some("I listed the dir")
        );

        // tools passed through
        assert_eq!(t.tools.len(), 1);
    }

    #[test]
    fn missing_messages_is_malformed() {
        let err = reconstruct(&json!({}), &json!({})).unwrap_err();
        assert!(matches!(err, ExportError::Malformed("openai-chat", _)));
    }
}
