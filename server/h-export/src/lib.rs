//! Reconstruct a captured agent turn/session into a **training trajectory**
//! shaped for SFT fine-tuning (OpenAI `messages` format).
//!
//! The agent CLI re-sends the full cumulative message history on every call, so
//! a turn's terminal call already carries the whole turn in its `request_body`,
//! and a session's last turn's terminal call carries the whole session. We
//! therefore reconstruct from a single (request_body, response_body) pair:
//! `messages = normalize(request_body) ++ final-assistant(response_body)`.
//!
//! Output invariants (OpenAI-style SFT): `messages[]` each have a `role`;
//! `tool_calls[].function.arguments` is always a **dict, never a JSON string**;
//! assistant `reasoning_content` (Anthropic `thinking` / OpenAI `reasoning`) is
//! preserved. `tools` and `_meta` are tolerated extras.

use serde::Serialize;
use serde_json::{json, Value};

mod anthropic;
mod openai_chat;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Wire formats not yet supported by the normalizer (codex
    /// `openai-responses`, `gemini-aistudio`). Adding one is a new dispatch arm.
    #[error("unsupported wire_api: {0}")]
    UnsupportedWireApi(String),
    /// The terminal call's body was sampled away by the capture body cap
    /// (`body_bytes_dropped > 0`) — reconstructing would silently drop turns.
    #[error("captured body was truncated (body_bytes_dropped > 0); trajectory would be lossy")]
    BodyTruncated,
    /// No terminal call / no body to reconstruct from.
    #[error("no terminal call body to reconstruct from")]
    MissingTerminalBody,
    #[error("malformed {0} body: {1}")]
    Malformed(&'static str, String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Always a JSON object. OpenAI stores this as a string on the wire; the
    /// normalizer parses it so the exported tool call is the expected dict shape.
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrainingTrajectory {
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    /// Provenance, set by the caller via [`TrainingTrajectory::with_meta`].
    #[serde(rename = "_meta")]
    pub meta: Value,
}

impl TrainingTrajectory {
    /// Attach a provenance `_meta` object (the normalizer leaves it `{}`).
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = meta;
        self
    }
}

/// Reconstruct a trajectory from one captured (request, response) body pair.
/// Bodies must already be parsed JSON (heron stores reassembled JSON even for
/// streaming). `_meta` is left empty — attach it with [`TrainingTrajectory::with_meta`].
pub fn reconstruct_trajectory(
    wire_api: &str,
    request_body: &Value,
    response_body: &Value,
) -> Result<TrainingTrajectory, ExportError> {
    match wire_api {
        h_llm::wire_apis::OPENAI_CHAT => openai_chat::reconstruct(request_body, response_body),
        h_llm::wire_apis::ANTHROPIC => anthropic::reconstruct(request_body, response_body),
        other => Err(ExportError::UnsupportedWireApi(other.to_string())),
    }
}

// ---- shared helpers used by both wire normalizers --------------------------

/// Parse a tool-call `arguments` field into a dict. OpenAI sends a JSON
/// **string**; Anthropic `tool_use.input` is already an object. Unparseable
/// strings degrade to `{}` (valid, lossy) rather than failing the export.
pub(crate) fn arguments_to_dict(args: Option<&Value>) -> Value {
    match args {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_wire_returns_typed_error() {
        let v = json!({});
        for wire in ["openai-responses", "gemini-aistudio", "totally-unknown"] {
            match reconstruct_trajectory(wire, &v, &v) {
                Err(ExportError::UnsupportedWireApi(w)) => assert_eq!(w, wire),
                other => panic!("expected UnsupportedWireApi for {wire}, got {other:?}"),
            }
        }
    }

    #[test]
    fn arguments_string_becomes_dict() {
        let parsed = arguments_to_dict(Some(&json!("{\"k\":\"v\",\"n\":3}")));
        assert!(parsed.is_object());
        assert_eq!(parsed["k"], json!("v"));
        assert_eq!(parsed["n"], json!(3));
        // already-dict passes through
        assert_eq!(arguments_to_dict(Some(&json!({"a":1}))), json!({"a":1}));
        // garbage / absent → empty dict
        assert_eq!(arguments_to_dict(Some(&json!("not json"))), json!({}));
        assert_eq!(arguments_to_dict(None), json!({}));
    }
}
