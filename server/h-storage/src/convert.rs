//! Backend-neutral value conversions shared by every `StorageBackend`
//! implementation: header JSON (de)serialization, JSON string-list parsing,
//! and the wire-vs-estimated token heuristic. Pure functions with no SQL
//! dialect or driver dependency, so DuckDB / ClickHouse / future backends all
//! call the same copy (single source of truth).

/// Decide whether a row's `(input_tokens, output_tokens)` came from the
/// fallback estimator vs the wire `usage` block. Returns true when the row
/// has any tokens AND the response body either lacks a `usage` object or
/// every numeric field inside `usage` is zero. Wire-api-agnostic — looks for
/// any of the four canonical fields under `usage` (OpenAI Chat / Anthropic
/// / OpenAI Responses all use one of these names).
pub fn derive_tokens_estimated(
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    response_body: Option<&str>,
) -> bool {
    let in_tok = input_tokens.unwrap_or(0);
    let out_tok = output_tokens.unwrap_or(0);
    if in_tok == 0 && out_tok == 0 {
        return false;
    }
    let body = match response_body {
        Some(s) if !s.is_empty() => s,
        _ => return true,
    };
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        _ => return true,
    };
    let usage = match v.get("usage") {
        Some(u) if u.is_object() => u,
        _ => return true,
    };
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "input_tokens",
        "output_tokens",
    ] {
        if let Some(n) = usage.get(key).and_then(|v| v.as_u64()) {
            if n > 0 {
                return false;
            }
        }
    }
    true
}

/// Serialize HTTP headers as a JSON array of pairs.
/// Output format: `[["content-type","application/json"],["x-request-id","req_xxx"]]`
/// Preserves header order and allows duplicate keys.
pub fn headers_to_json(headers: &[(String, String)]) -> String {
    use serde_json::Value;
    let pairs: Vec<Value> = headers
        .iter()
        .map(|(k, v)| Value::Array(vec![Value::String(k.clone()), Value::String(v.clone())]))
        .collect();
    Value::Array(pairs).to_string()
}

/// Parse a JSON-encoded array-of-strings (as stored in agent_turns.models_used /
/// subagents_used / span_ids) into a `Vec<String>`. Missing or malformed values
/// degrade to an empty vec — the turn payload is still returnable.
pub fn parse_json_string_list(raw: Option<&str>) -> Vec<String> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod derive_tokens_estimated_tests {
    use super::derive_tokens_estimated;

    #[test]
    fn zero_tokens_returns_false() {
        assert!(!derive_tokens_estimated(Some(0), Some(0), None));
        assert!(!derive_tokens_estimated(None, None, Some(r#"{"x":1}"#)));
    }

    #[test]
    fn no_body_with_tokens_returns_true() {
        assert!(derive_tokens_estimated(Some(10), Some(5), None));
        assert!(derive_tokens_estimated(Some(10), Some(5), Some("")));
    }

    #[test]
    fn malformed_body_with_tokens_returns_true() {
        assert!(derive_tokens_estimated(Some(10), Some(5), Some("not json")));
    }

    #[test]
    fn body_with_positive_usage_returns_false() {
        let body = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        assert!(!derive_tokens_estimated(Some(10), Some(5), Some(body)));
    }

    #[test]
    fn body_with_zero_usage_returns_true() {
        let body = r#"{"usage":{"prompt_tokens":0,"completion_tokens":0}}"#;
        assert!(derive_tokens_estimated(Some(10), Some(5), Some(body)));
    }

    #[test]
    fn anthropic_shape_recognized() {
        let body = r#"{"usage":{"input_tokens":7,"output_tokens":3}}"#;
        assert!(!derive_tokens_estimated(Some(7), Some(3), Some(body)));
    }

    #[test]
    fn body_missing_usage_block_returns_true() {
        let body = r#"{"choices":[{"message":{"content":"hi"}}]}"#;
        assert!(derive_tokens_estimated(Some(5), Some(2), Some(body)));
    }
}
