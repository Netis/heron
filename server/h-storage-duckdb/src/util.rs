//! DuckDB-specific cross-cutting helpers: SQL TIMESTAMP literal formatting,
//! body extraction (LlmCall → profile-driven user/assistant text), and DuckDB
//! `Value` constructors.
//!
//! Backend-neutral helpers (JSON header (de)serialization, the wire-vs-
//! estimated token heuristic, and the dimension-filter SQL builders) live in
//! `h_storage::convert` / `h_storage::dialect` and are re-exported here so the
//! existing `crate::util::*` call sites stay unchanged.

use std::time::SystemTime;

use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use h_common::error::{AppError, Result};
use h_llm::agents::build_default_registry;
use h_llm::model::{ApiType, LlmCall};
use h_llm::profile::{parse_bodies, CallCtx};
use h_llm::wire_apis as wa;

pub(crate) use h_storage::convert::{
    derive_tokens_estimated, headers_to_json, parse_json_string_list,
};
pub(crate) use h_storage::dialect::{
    build_dimension_where, build_dimension_where_for_group, escape_standard, sql_in_list,
};

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

/// Load the request_body / response_body of `call_id` from spans and run
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
        ExtractKind::User => "SELECT request_body, wire_api FROM spans WHERE id = ?",
        ExtractKind::Assistant => "SELECT response_body, wire_api FROM spans WHERE id = ?",
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
        body_bytes_dropped: 0,
        attribution: h_common::attribution::AttributionInfo::ambiguous(),
        process: None,
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
/// `SELECT ... WHERE id IN (...)` against `spans` and runs each profile's
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
    // appears twice — extremely unlikely given Trace invariants).
    let mut agent_by_call: HashMap<&str, &str> = HashMap::new();
    for (ak, cid) in requests {
        agent_by_call.insert(cid.as_str(), ak.as_str());
    }
    let span_ids: Vec<&str> = agent_by_call.keys().copied().collect();

    let col = match kind {
        ExtractKind::User => "request_body",
        ExtractKind::Assistant => "response_body",
    };
    let placeholders = vec!["?"; span_ids.len()].join(",");
    let sql = format!("SELECT id, wire_api, {col} FROM spans WHERE id IN ({placeholders})");

    let registry = build_default_registry();

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let params: Vec<&dyn duckdb::ToSql> =
        span_ids.iter().map(|s| s as &dyn duckdb::ToSql).collect();
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
            body_bytes_dropped: 0,
            attribution: h_common::attribution::AttributionInfo::ambiguous(),
            process: None,
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

/// Convert a `SystemTime` into a DuckDB microsecond-precision timestamp value.
pub(crate) fn timestamp_value(t: SystemTime) -> Result<Value> {
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| AppError::Storage(format!("retention cutoff before UNIX epoch: {e}")))?;
    let micros = i64::try_from(dur.as_micros())
        .map_err(|_| AppError::Storage("retention cutoff out of i64 range".to_string()))?;
    Ok(Value::Timestamp(TimeUnit::Microsecond, micros))
}
