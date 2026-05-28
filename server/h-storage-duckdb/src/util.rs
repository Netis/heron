//! Cross-cutting helpers used by multiple entity modules:
//! JSON header serialization, time conversion, body extraction (LlmCall →
//! profile-driven user/assistant text), generic SQL fragment builders, and
//! DuckDB `Value` constructors.

use std::time::SystemTime;

use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use h_common::error::{AppError, Result};
use h_llm::agents::build_default_registry;
use h_llm::model::{ApiType, LlmCall};
use h_llm::profile::{parse_bodies, CallCtx};
use h_llm::wire_apis as wa;
use h_storage::query::DimensionFilter;

/// Decide whether a row's `(input_tokens, output_tokens)` came from the
/// fallback estimator vs the wire `usage` block. Returns true when the row
/// has any tokens AND the response body either lacks a `usage` object or
/// every numeric field inside `usage` is zero. Wire-api-agnostic — looks for
/// any of the four canonical fields under `usage` (OpenAI Chat / Anthropic
/// / OpenAI Responses all use one of these names).
pub(crate) fn derive_tokens_estimated(
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
pub(crate) fn headers_to_json(headers: &[(String, String)]) -> String {
    use serde_json::Value;
    let pairs: Vec<Value> = headers
        .iter()
        .map(|(k, v)| Value::Array(vec![Value::String(k.clone()), Value::String(v.clone())]))
        .collect();
    Value::Array(pairs).to_string()
}

/// Convert microseconds since epoch to a string DuckDB can parse as TIMESTAMP.
///
/// DuckDB's SQL TIMESTAMP parser only accepts 4-digit years (`YYYY-MM-DD`);
/// anything year >9999 formatted by chrono comes out as `+58346-09-15...`
/// which the parser then rejects with a Conversion Error and crashes the
/// whole query. Defense-in-depth: clamp `us` to the year-9999 boundary so
/// a stray over-large value (already validated at the API boundary, but
/// this is the last line of defense before the SQL string is built) can't
/// produce a parse-failing SQL literal.
const MAX_DUCKDB_PARSABLE_US: i64 = 253_402_300_799_000_000; // 9999-12-31 23:59:59 UTC

pub(crate) fn us_to_timestamp(us: i64) -> String {
    let us = us.clamp(0, MAX_DUCKDB_PARSABLE_US);
    let secs = us / 1_000_000;
    let micros = (us.rem_euclid(1_000_000)) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, micros * 1000).unwrap_or_default();
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

#[cfg(test)]
mod us_to_timestamp_tests {
    use super::us_to_timestamp;

    #[test]
    fn normal_value_formats_ok() {
        // 1_747_000_000_000_000 us = 1_747_000_000 sec = 2025-05-11 21:46:40 UTC
        assert_eq!(
            us_to_timestamp(1_747_000_000_000_000),
            "2025-05-11 21:46:40.000000"
        );
    }

    #[test]
    fn over_year_9999_clamped_not_corrupted() {
        // Caller bug: passed something like ns where us was expected.
        // Year ~58000 would crash DuckDB's parser; clamp to 9999 instead.
        let out = us_to_timestamp(1_842_000_000_000_000_000);
        assert!(out.starts_with("9999-"), "expected clamp, got {out}");
    }

    #[test]
    fn negative_clamped_to_epoch() {
        assert_eq!(us_to_timestamp(-1), "1970-01-01 00:00:00.000000");
    }
}

/// Parse a JSON-encoded array-of-strings (as stored in agent_turns.models_used /
/// subagents_used / call_ids) into a `Vec<String>`. Missing or malformed values
/// degrade to an empty vec — the turn payload is still returnable.
pub(crate) fn parse_json_string_list(raw: Option<&str>) -> Vec<String> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub(crate) enum ExtractKind {
    User,
    Assistant,
}

/// Render a BLOB body for the HTTP exchange detail API. UTF-8 text passes
/// through; binary content (gzip, protobuf, …) falls back to a placeholder so
/// the detail page reflects that bytes were captured rather than showing
/// blank.
pub(crate) fn render_body_for_detail(bytes: Option<Vec<u8>>) -> Option<String> {
    let b = bytes?;
    match String::from_utf8(b) {
        Ok(s) => Some(s),
        Err(e) => Some(format!("[binary, {} bytes]", e.into_bytes().len())),
    }
}

/// Load the request_body / response_body of `call_id` from llm_calls and run
/// it through the `agent_kind`-matched profile to produce the full user_input
/// or final_answer text. Returns `None` if the call row is missing, the
/// profile is not registered, or the extractor declines.
pub(crate) fn extract_full_text(
    conn: &Connection,
    agent_kind: &str,
    call_id: Option<&str>,
    kind: ExtractKind,
) -> Option<String> {
    let call_id = call_id?;
    let registry = build_default_registry();
    let profile = registry.find_by_name(agent_kind)?;

    let sql = match kind {
        ExtractKind::User => "SELECT request_body, wire_api FROM llm_calls WHERE id = ?",
        ExtractKind::Assistant => "SELECT response_body, wire_api FROM llm_calls WHERE id = ?",
    };
    let (body, wire_api_stored): (Option<String>, String) = conn
        .query_row(sql, duckdb::params![call_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .ok()?;
    // Resolve the stored value back to its static constant. An unknown value
    // means the DB has a wire_api this binary no longer knows about — drop
    // the extraction rather than fabricate one.
    let wire_api = wa::by_name(&wire_api_stored)?;
    let (request_body, response_body) = match kind {
        ExtractKind::User => (body, None),
        ExtractKind::Assistant => (None, body),
    };

    // Placeholder LlmCall carrying the real wire_api + bodies; other fields
    // are defaulted because current extractors only read these.
    let call = LlmCall {
        source_id: String::new(),
        id: String::new(),
        wire_api,
        model: String::new(),
        api_type: ApiType::Chat,
        request_time: 0,
        response_time: None,
        complete_time: None,
        request_path: String::new(),
        is_stream: false,
        request_body,
        status_code: None,
        finish_reason: None,
        response_body,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        ttft_ms: None,
        e2e_latency_ms: None,
        client_ip: "0.0.0.0".parse().unwrap(),
        client_port: 0,
        server_ip: "0.0.0.0".parse().unwrap(),
        server_port: 0,
        response_id: None,
        request_headers: Vec::new(),
        response_headers: Vec::new(),
        is_agent_request: false,
        tool_surface: None,
        agent_topology: None,
        tool_call_count: 0,
        tool_names: vec![],
    };
    let (req, resp) = parse_bodies(&call);
    let ctx = CallCtx::new(&call, req.as_ref(), resp.as_ref());
    match kind {
        ExtractKind::User => profile.extract_user_input(&ctx),
        ExtractKind::Assistant => profile.extract_assistant_text(&ctx),
    }
}

/// Batch version of `extract_full_text`. Given `(agent_kind, call_id)` pairs
/// and an `ExtractKind` selecting which body column to read, issues a single
/// `SELECT ... WHERE id IN (...)` against `llm_calls` and runs each profile's
/// extractor to produce the final text. Returns a map keyed by `call_id`.
///
/// - Missing call rows, unknown `wire_api`s, or extractors that decline are
///   omitted from the result (caller falls back to the preview string).
/// - Empty `requests` short-circuits to an empty map with zero DB work.
pub(crate) fn extract_full_text_batch(
    conn: &Connection,
    kind: ExtractKind,
    requests: &[(String, String)], // (agent_kind, call_id)
) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut out: HashMap<String, String> = HashMap::new();
    if requests.is_empty() {
        return out;
    }

    // Build agent_kind lookup keyed by call_id (last-writer-wins if a call id
    // appears twice — extremely unlikely given AgentTurn invariants).
    let mut agent_by_call: HashMap<&str, &str> = HashMap::new();
    for (ak, cid) in requests {
        agent_by_call.insert(cid.as_str(), ak.as_str());
    }
    let call_ids: Vec<&str> = agent_by_call.keys().copied().collect();

    let col = match kind {
        ExtractKind::User => "request_body",
        ExtractKind::Assistant => "response_body",
    };
    let placeholders = vec!["?"; call_ids.len()].join(",");
    let sql = format!("SELECT id, wire_api, {col} FROM llm_calls WHERE id IN ({placeholders})");

    let registry = build_default_registry();

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let params: Vec<&dyn duckdb::ToSql> =
        call_ids.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let Ok(mut rows) = stmt.query(duckdb::params_from_iter(params.iter().copied())) else {
        return out;
    };

    while let Ok(Some(row)) = rows.next() {
        let Ok(id): std::result::Result<String, _> = row.get(0) else {
            continue;
        };
        let Ok(wire_api_stored): std::result::Result<String, _> = row.get(1) else {
            continue;
        };
        let body: Option<String> = row.get(2).ok();
        let Some(wire_api) = wa::by_name(&wire_api_stored) else {
            continue;
        };
        let Some(agent_kind) = agent_by_call.get(id.as_str()).copied() else {
            continue;
        };
        let Some(profile) = registry.find_by_name(agent_kind) else {
            continue;
        };

        let (request_body, response_body) = match kind {
            ExtractKind::User => (body, None),
            ExtractKind::Assistant => (None, body),
        };
        let call = LlmCall {
            source_id: String::new(),
            id: String::new(),
            wire_api,
            model: String::new(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: String::new(),
            is_stream: false,
            request_body,
            status_code: None,
            finish_reason: None,
            response_body,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "0.0.0.0".parse().unwrap(),
            client_port: 0,
            server_ip: "0.0.0.0".parse().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
        };
        let (req, resp) = parse_bodies(&call);
        let ctx = CallCtx::new(&call, req.as_ref(), resp.as_ref());
        let extracted = match kind {
            ExtractKind::User => profile.extract_user_input(&ctx),
            ExtractKind::Assistant => profile.extract_assistant_text(&ctx),
        };
        if let Some(text) = extracted {
            out.insert(id, text);
        }
    }

    out
}

/// Format a list of string values as a SQL IN list with single-quote escaping.
pub(crate) fn sql_in_list(values: &[String]) -> String {
    values
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a WHERE clause segment for dimension filters on an ungrouped query.
///
/// The aggregator (see `h-metrics/src/aggregator.rs:dimension_keys`) only
/// materializes 4 of the 8 possible wildcard combinations for
/// `(wire_api, model, server_ip)`:
///
/// - `(W, M, S)` — finest
/// - `(W, M, *)` — per (wire_api, model), summed across servers
/// - `(*, *, S)` — per server_ip only
/// - `(*, *, *)` — grand total
///
/// The mapping below picks the coarsest tier that covers the user's filter
/// and SUMs across the remaining rows. A filter on wire_api or model forces
/// us below the `(*, *, ·)` tier; a filter on server_ip forces us off the
/// `server_ip = '*'` coordinate.
pub(crate) fn build_dimension_where(filter: &DimensionFilter) -> String {
    let has_wire = !filter.wire_apis.is_empty();
    let has_model = !filter.models.is_empty();
    let has_server = !filter.server_ips.is_empty();

    let (wire_clause, model_clause) = if !has_wire && !has_model {
        // Stay on (*, *, ·) tier.
        ("wire_api = '*'".to_string(), "model = '*'".to_string())
    } else {
        // Drop to (W, M, ·) tier — either IN-list or all specific values.
        let w = if has_wire {
            format!("wire_api IN ({})", sql_in_list(&filter.wire_apis))
        } else {
            "wire_api != '*'".to_string()
        };
        let m = if has_model {
            format!("model IN ({})", sql_in_list(&filter.models))
        } else {
            "model != '*'".to_string()
        };
        (w, m)
    };

    let server_clause = if has_server {
        format!("server_ip IN ({})", sql_in_list(&filter.server_ips))
    } else {
        "server_ip = '*'".to_string()
    };

    let surface_clause = build_tool_surface_clause(&filter.tool_surfaces);
    format!("{wire_clause} AND {model_clause} AND {server_clause}{surface_clause}")
}

/// Build WHERE clause for queries that GROUP BY `wire_api` or `model`. The
/// group dimension is always forced to a specific value (never `'*'`); the
/// remaining dimensions follow the same filter/tier rules as
/// [`build_dimension_where`]. Any non-recognized `group_by` falls through to
/// the ungrouped builder.
pub(crate) fn build_dimension_where_for_group(filter: &DimensionFilter, group_by: &str) -> String {
    match group_by {
        "wire_api" | "model" => {
            let wire_clause = if !filter.wire_apis.is_empty() {
                format!("wire_api IN ({})", sql_in_list(&filter.wire_apis))
            } else {
                "wire_api != '*'".to_string()
            };
            let model_clause = if !filter.models.is_empty() {
                format!("model IN ({})", sql_in_list(&filter.models))
            } else {
                "model != '*'".to_string()
            };
            let server_clause = if !filter.server_ips.is_empty() {
                format!("server_ip IN ({})", sql_in_list(&filter.server_ips))
            } else {
                "server_ip = '*'".to_string()
            };
            let surface_clause = build_tool_surface_clause(&filter.tool_surfaces);
            format!("{wire_clause} AND {model_clause} AND {server_clause}{surface_clause}")
        }
        _ => build_dimension_where(filter),
    }
}

/// Optional `tool_surface IN (...)` segment, prefixed with ` AND ` when
/// present. Returns an empty string when no filter is set so the query stays
/// unchanged and rolls up across all surfaces (including NULL).
fn build_tool_surface_clause(surfaces: &[String]) -> String {
    if surfaces.is_empty() {
        String::new()
    } else {
        format!(" AND tool_surface IN ({})", sql_in_list(surfaces))
    }
}

/// Convert a `SystemTime` into a DuckDB microsecond-precision timestamp value.
pub(crate) fn timestamp_value(t: SystemTime) -> Result<Value> {
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| AppError::Storage(format!("retention cutoff before UNIX epoch: {e}")))?;
    let micros = i64::try_from(dur.as_micros())
        .map_err(|_| AppError::Storage("retention cutoff out of i64 range".to_string()))?;
    Ok(Value::Timestamp(TimeUnit::Microsecond, micros))
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

#[cfg(test)]
mod build_dimension_where_tests {
    use super::*;

    #[test]
    fn test_build_dimension_where_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where(&f),
            "wire_api = '*' AND model = '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_server_only() {
        let f = DimensionFilter {
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api = '*' AND model = '*' AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_only() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model != '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_model_only() {
        let f = DimensionFilter {
            models: vec!["gpt-4".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api != '*' AND model IN ('gpt-4') AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_and_model() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            models: vec!["gpt-4".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model IN ('gpt-4') AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_and_server() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_model_and_server() {
        let f = DimensionFilter {
            models: vec!["gpt-4".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api != '*' AND model IN ('gpt-4') AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_all_three() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            models: vec!["gpt-4".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model IN ('gpt-4') AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_for_group_wire_api_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where_for_group(&f, "wire_api"),
            "wire_api != '*' AND model != '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_for_group_with_server_filter() {
        let f = DimensionFilter {
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where_for_group(&f, "wire_api"),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
        assert_eq!(
            build_dimension_where_for_group(&f, "model"),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
    }
}
