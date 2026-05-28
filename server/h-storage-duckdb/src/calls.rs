//! `llm_calls` table I/O — write, query (paginated / by-id / by-id-list).

use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use h_common::error::{AppError, Result};
use h_llm::model::LlmCall;
use h_storage::query::*;

use crate::util::{
    derive_tokens_estimated, headers_to_json, parse_json_string_list, us_to_timestamp,
};
use crate::DuckDbBackend;

/// Bindable row prepared outside the writer Mutex.
/// All expensive conversions (IP formatting, enum → string, header JSON,
/// timestamp wrapping) happen before the lock is acquired.
struct PreparedCall {
    id: String,
    source_id: String,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    request_time: Value,
    response_time: Option<Value>,
    complete_time: Option<Value>,
    wire_api: String,
    model: String,
    api_type: String,
    is_stream: bool,
    request_path: String,
    status_code: Option<u16>,
    finish_reason: Option<String>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    total_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
    ttft_ms: Option<f64>,
    e2e_latency_ms: Option<f64>,
    request_body: Option<String>,
    response_body: Option<String>,
    response_id: Option<String>,
    request_headers: String,
    response_headers: String,
    is_agent_request: bool,
    tool_surface: Option<String>,
    agent_topology: Option<String>,
    tool_call_count: u32,
    tool_names_json: String,
}

fn prepare_call(call: LlmCall) -> PreparedCall {
    PreparedCall {
        id: call.id,
        source_id: call.source_id,
        client_ip: call.client_ip.to_string(),
        client_port: call.client_port,
        server_ip: call.server_ip.to_string(),
        server_port: call.server_port,
        request_time: Value::Timestamp(TimeUnit::Microsecond, call.request_time),
        response_time: call
            .response_time
            .map(|us| Value::Timestamp(TimeUnit::Microsecond, us)),
        complete_time: call
            .complete_time
            .map(|us| Value::Timestamp(TimeUnit::Microsecond, us)),
        wire_api: call.wire_api.to_string(),
        model: call.model,
        api_type: call.api_type.to_string(),
        is_stream: call.is_stream,
        request_path: call.request_path,
        status_code: call.status_code,
        finish_reason: call.finish_reason,
        input_tokens: call.input_tokens,
        output_tokens: call.output_tokens,
        total_tokens: call.total_tokens,
        cache_read_input_tokens: call.cache_read_input_tokens,
        cache_creation_input_tokens: call.cache_creation_input_tokens,
        ttft_ms: call.ttft_ms,
        e2e_latency_ms: call.e2e_latency_ms,
        request_body: call.request_body,
        response_body: call.response_body,
        response_id: call.response_id,
        request_headers: headers_to_json(&call.request_headers),
        response_headers: headers_to_json(&call.response_headers),
        is_agent_request: call.is_agent_request,
        tool_surface: call.tool_surface.map(|s| s.to_string()),
        agent_topology: call.agent_topology.map(|s| s.to_string()),
        tool_call_count: call.tool_call_count,
        tool_names_json: serde_json::to_string(&call.tool_names)
            .unwrap_or_else(|_| "[]".to_string()),
    }
}

/// Shared "fetch calls by id list" — used by both `query_turn_calls`
/// (which derives the ids from the persisted `agent_turns.call_ids`)
/// and `query_calls_by_ids` (which receives the ids directly from the
/// API for in-progress turns whose call_ids live in the in-memory
/// active-turn registry). Calls not yet flushed from `WriteBuffer` to
/// `llm_calls` simply don't return — caller treats that as "show
/// fewer rows on this refresh, more on the next one."
fn read_calls_by_ids_sync(
    conn: &Connection,
    call_ids: &[String],
    include_bodies: bool,
) -> Result<Vec<TurnCallItem>> {
    if call_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", call_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    // Lite mode (`include_bodies = false`) selects `NULL` for the four
    // heavy fields directly in SQL, so DuckDB never reads the body
    // pages off disk and the rows never need to be transferred to
    // Rust as Strings. The downstream `tokens_estimated` derivation
    // falls back to `false` when response_body is missing — documented
    // on `StorageBackend::query_turn_calls`.
    let body_columns: &str = if include_bodies {
        "request_body, response_body, request_headers, response_headers"
    } else {
        "NULL::VARCHAR AS request_body, \
         NULL::VARCHAR AS response_body, \
         NULL::VARCHAR AS request_headers, \
         NULL::VARCHAR AS response_headers"
    };
    let sql = format!(
        "SELECT
            id,
            epoch_ms(request_time),
            epoch_ms(response_time),
            epoch_ms(complete_time),
            wire_api, model, status_code, is_stream,
            finish_reason, ttft_ms, e2e_latency_ms,
            input_tokens, output_tokens,
            request_path, client_ip, client_port,
            server_ip, server_port,
            {body_columns}
        FROM llm_calls
        WHERE id IN ({placeholders})
        ORDER BY request_time ASC, complete_time ASC"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AppError::Storage(format!("failed to prepare turn_calls step2: {e}")))?;
    let mut rows = stmt
        .query(duckdb::params_from_iter(call_ids.iter()))
        .map_err(|e| AppError::Storage(format!("failed to execute turn_calls step2: {e}")))?;

    let mut items = Vec::new();
    let mut seq: u32 = 0;
    while let Some(row) = rows
        .next()
        .map_err(|e| AppError::Storage(format!("row error: {e}")))?
    {
        seq += 1;
        items.push(TurnCallItem {
            id: row
                .get(0)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            sequence: seq,
            request_time: row
                .get(1)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            response_time: row
                .get::<_, Option<i64>>(2)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            complete_time: row
                .get::<_, Option<i64>>(3)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            wire_api: row
                .get(4)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            model: row
                .get(5)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            status_code: row
                .get::<_, Option<u16>>(6)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            is_stream: row
                .get(7)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            finish_reason: row
                .get::<_, Option<String>>(8)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            ttft_ms: row
                .get::<_, Option<f64>>(9)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            e2e_latency_ms: row
                .get::<_, Option<f64>>(10)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            input_tokens: {
                let v: Option<u32> = row
                    .get(11)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                v
            },
            output_tokens: {
                let v: Option<u32> = row
                    .get(12)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                v
            },
            tokens_estimated: false, // overwritten below once we read response_body
            request_path: row
                .get(13)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            client_ip: row
                .get(14)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            client_port: row
                .get(15)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            server_ip: row
                .get(16)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            server_port: row
                .get(17)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            request_body: row
                .get::<_, Option<String>>(18)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            response_body: row
                .get::<_, Option<String>>(19)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            request_headers: row
                .get::<_, Option<String>>(20)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            response_headers: row
                .get::<_, Option<String>>(21)
                .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
        });
        let last = items.last_mut().expect("just pushed");
        last.tokens_estimated = derive_tokens_estimated(
            last.input_tokens,
            last.output_tokens,
            last.response_body.as_deref(),
        );
    }
    Ok(items)
}

impl DuckDbBackend {
    pub(crate) async fn write_calls(&self, calls: Vec<LlmCall>) -> Result<()> {
        if calls.is_empty() {
            return Ok(());
        }
        #[cfg(feature = "fault-injection")]
        {
            use crate::fault_injection::FaultPoint;
            if self.fault_set.should_fire(FaultPoint::DuckDbInvalidate) {
                return Err(crate::fault_injection::fatal_invalidate_error());
            }
            if self.fault_set.should_fire(FaultPoint::DiskFull) {
                return Err(crate::fault_injection::disk_full_error());
            }
        }
        let conn = self.write_calls_conn.clone();
        tokio::task::spawn_blocking(move || {
            // Serialize/format outside the writer Mutex so the lock is held
            // only for the append + flush.
            let prepared: Vec<PreparedCall> = calls.into_iter().map(prepare_call).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("llm_calls")
                .map_err(|e| AppError::Storage(format!("failed to create appender: {e}")))?;
            for p in &prepared {
                appender
                    .append_row(duckdb::params![
                        p.id,
                        p.source_id,
                        p.client_ip,
                        p.client_port,
                        p.server_ip,
                        p.server_port,
                        p.request_time,
                        p.response_time,
                        p.complete_time,
                        p.wire_api,
                        p.model,
                        p.api_type,
                        p.is_stream,
                        p.request_path,
                        p.status_code,
                        p.finish_reason,
                        p.input_tokens,
                        p.output_tokens,
                        p.total_tokens,
                        p.cache_read_input_tokens,
                        p.cache_creation_input_tokens,
                        p.ttft_ms,
                        p.e2e_latency_ms,
                        p.request_body,
                        p.response_body,
                        p.response_id,
                        p.request_headers,
                        p.response_headers,
                        p.is_agent_request,
                        p.tool_surface,
                        p.agent_topology,
                        p.tool_call_count,
                        p.tool_names_json,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append call: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush calls: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage> {
        const VALID_SORT_FIELDS: &[&str] = &[
            "request_time",
            "status_code",
            "ttft_ms",
            "e2e_latency_ms",
            "input_tokens",
            "output_tokens",
        ];

        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.to_uppercase() == "ASC" {
            "ASC"
        } else {
            "DESC"
        };

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();
        let sort_order = sort_order.to_string();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            // Build WHERE clauses
            let mut where_parts = vec![
                "request_time >= ?".to_string(),
                "request_time < ?".to_string(),
            ];

            if !query.filter.wire_apis.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .wire_apis
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("wire_api IN ({})", list.join(", ")));
            }
            if !query.filter.models.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .models
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("model IN ({})", list.join(", ")));
            }
            if !query.filter.server_ips.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .server_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("server_ip IN ({})", list.join(", ")));
            }
            if !query.status_codes.is_empty() {
                let list: Vec<String> = query.status_codes.iter().map(|c| c.to_string()).collect();
                where_parts.push(format!("status_code IN ({})", list.join(", ")));
            }
            if !query.finish_reasons.is_empty() {
                let list: Vec<String> = query
                    .finish_reasons
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("finish_reason IN ({})", list.join(", ")));
            }
            if !query.client_ips.is_empty() {
                let list: Vec<String> = query
                    .client_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("client_ip IN ({})", list.join(", ")));
            }
            if !query.server_ports.is_empty() {
                let list: Vec<String> = query
                    .server_ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect();
                where_parts.push(format!("server_port IN ({})", list.join(", ")));
            }
            if let Some(substr) = query
                .request_path_contains
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                where_parts.push(format!(
                    "request_path LIKE '%{}%'",
                    substr.replace('\'', "''")
                ));
            }
            if let Some(stream) = query.is_stream {
                where_parts.push(format!("is_stream = {stream}"));
            }
            let where_sql = where_parts.join(" AND ");
            let sort_by = &query.sort_by;

            // COUNT query
            let count_sql = format!("SELECT COUNT(*) FROM llm_calls WHERE {where_sql}");
            let mut count_stmt = conn
                .prepare(&count_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare count query: {e}")))?;
            let total: u64 = count_stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| row.get(0))
                .map_err(|e| AppError::Storage(format!("failed to execute count query: {e}")))?;

            // Items query
            let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
            let limit = query.page_size;
            let items_sql = format!(
                "SELECT id, source_id, epoch_ms(request_time), wire_api, model, status_code, is_stream, \
                 finish_reason, ttft_ms, e2e_latency_ms, input_tokens, output_tokens, \
                 client_ip, server_ip, server_port, request_path, response_body, \
                 is_agent_request, tool_surface, agent_topology, tool_call_count, tool_names_json \
                 FROM llm_calls WHERE {where_sql} \
                 ORDER BY {sort_by} {sort_order} \
                 LIMIT {limit} OFFSET {offset}"
            );

            let mut items_stmt = conn
                .prepare(&items_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare items query: {e}")))?;

            let mut items = Vec::new();
            let mut query_rows = items_stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute items query: {e}")))?;

            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let input_tokens = row
                    .get::<_, Option<u32>>(10)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let output_tokens = row
                    .get::<_, Option<u32>>(11)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let response_body: Option<String> = row
                    .get(16)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let tokens_estimated =
                    derive_tokens_estimated(input_tokens, output_tokens, response_body.as_deref());
                let tool_names_json: Option<String> = row
                    .get(21)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let tool_names: Vec<String> = tool_names_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                items.push(CallListItem {
                    id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    source_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_time: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    wire_api: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    model: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    status_code: row
                        .get::<_, Option<u16>>(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    is_stream: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    finish_reason: row
                        .get::<_, Option<String>>(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttft_ms: row
                        .get::<_, Option<f64>>(8)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    e2e_latency_ms: row
                        .get::<_, Option<f64>>(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    input_tokens,
                    output_tokens,
                    tokens_estimated,
                    client_ip: row
                        .get(12)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_ip: row
                        .get(13)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_port: row
                        .get(14)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_path: row
                        .get(15)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    is_agent_request: row
                        .get::<_, Option<bool>>(17)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?
                        .unwrap_or(false),
                    tool_surface: row
                        .get::<_, Option<String>>(18)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    agent_topology: row
                        .get::<_, Option<String>>(19)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    tool_call_count: row
                        .get::<_, Option<u32>>(20)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?
                        .unwrap_or(0),
                    tool_names,
                });
            }

            Ok(CallsPage { total, items })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>> {
        let conn = self.read_pool.acquire().await?;
        let id = id.to_string();

        tokio::task::spawn_blocking(move || {
            let sql = "
                SELECT
                    id, source_id,
                    epoch_ms(request_time),
                    epoch_ms(response_time),
                    epoch_ms(complete_time),
                    wire_api, model, api_type, is_stream, request_path,
                    status_code, finish_reason,
                    input_tokens, output_tokens, total_tokens,
                    ttft_ms, e2e_latency_ms,
                    response_id,
                    client_ip, client_port, server_ip, server_port,
                    request_body, response_body,
                    request_headers, response_headers,
                    is_agent_request, tool_surface, agent_topology,
                    tool_call_count, tool_names_json
                FROM llm_calls
                WHERE id = ?
                LIMIT 1
            ";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare call_by_id query: {e}"))
            })?;

            let result = stmt.query_row(duckdb::params![id], |row| {
                let input_tokens: Option<u32> = row.get(12)?;
                let output_tokens: Option<u32> = row.get(13)?;
                let response_body: Option<String> = row.get(23)?;
                let tokens_estimated =
                    derive_tokens_estimated(input_tokens, output_tokens, response_body.as_deref());
                let tool_names_json: Option<String> = row.get(30)?;
                let tool_names: Vec<String> = tool_names_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                Ok(CallDetail {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    request_time: row.get(2)?,
                    response_time: row.get(3)?,
                    complete_time: row.get(4)?,
                    wire_api: row.get(5)?,
                    model: row.get(6)?,
                    api_type: row.get(7)?,
                    is_stream: row.get(8)?,
                    request_path: row.get(9)?,
                    status_code: row.get(10)?,
                    finish_reason: row.get(11)?,
                    input_tokens,
                    output_tokens,
                    total_tokens: row.get(14)?,
                    tokens_estimated,
                    ttft_ms: row.get(15)?,
                    e2e_latency_ms: row.get(16)?,
                    response_id: row.get(17)?,
                    client_ip: row.get(18)?,
                    client_port: row.get(19)?,
                    server_ip: row.get(20)?,
                    server_port: row.get(21)?,
                    request_body: row.get(22)?,
                    response_body,
                    request_headers: row.get(24)?,
                    response_headers: row.get(25)?,
                    is_agent_request: row.get::<_, Option<bool>>(26)?.unwrap_or(false),
                    tool_surface: row.get(27)?,
                    agent_topology: row.get(28)?,
                    tool_call_count: row.get::<_, Option<u32>>(29)?.unwrap_or(0),
                    tool_names,
                })
            });

            match result {
                Ok(detail) => Ok(Some(detail)),
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(AppError::Storage(format!(
                    "failed to query call by id: {e}"
                ))),
            }
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_turn_calls(
        &self,
        turn_id: &str,
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        let conn = self.read_pool.acquire().await?;
        let turn_id = turn_id.to_string();

        tokio::task::spawn_blocking(move || {
            // Step 1: fetch the turn's call_ids by PK. JSON-array column is
            // parsed with the same helper as query_turn_by_id.
            let call_ids_raw: Option<String> = {
                let mut stmt = conn
                    .prepare("SELECT call_ids FROM agent_turns WHERE turn_id = ?")
                    .map_err(|e| {
                        AppError::Storage(format!("failed to prepare turn_calls step1: {e}"))
                    })?;
                match stmt.query_row(duckdb::params![turn_id], |row| {
                    row.get::<_, Option<String>>(0)
                }) {
                    Ok(v) => v,
                    Err(duckdb::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
                    Err(e) => {
                        return Err(AppError::Storage(format!(
                            "failed to execute turn_calls step1: {e}"
                        )));
                    }
                }
            };

            let call_ids = parse_json_string_list(call_ids_raw.as_deref());
            // Step 2 lives in a shared helper so `query_calls_by_ids` (used
            // by the API for in-progress turns whose call_ids come from
            // the in-memory registry) can reuse the exact same projection.
            read_calls_by_ids_sync(&conn, &call_ids, include_bodies)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_calls_by_ids(
        &self,
        call_ids: &[String],
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        if call_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read_pool.acquire().await?;
        let call_ids: Vec<String> = call_ids.to_vec();
        tokio::task::spawn_blocking(move || {
            read_calls_by_ids_sync(&conn, &call_ids, include_bodies)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use std::net::IpAddr;
    use h_llm::model::{ApiType, LlmCall};
    use h_llm::wire_apis as wa;
    use h_storage::query::*;
    use h_storage::StorageBackend;

    fn in_memory() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
    }

    fn sample_call() -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "01912345-6789-7abc-def0-123456789abc".to_string(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            request_time: 1_700_000_000_000_000,
            response_time: Some(1_700_000_000_500_000),
            complete_time: Some(1_700_000_001_000_000),
            request_path: "/v1/chat/completions".to_string(),
            is_stream: true,
            request_body: Some(r#"{"model":"gpt-4"}"#.to_string()),
            status_code: Some(200),
            finish_reason: Some("stop".to_string()),
            response_body: Some(r#"{"choices":[...]}"#.to_string()),
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: Some(500.0),
            e2e_latency_ms: Some(1000.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 54321,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 8080,
            response_id: Some("chatcmpl-test123".to_string()),
            request_headers: vec![
                ("authorization".to_string(), "Bearer sk-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            response_headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-request-id".to_string(), "req_abc123".to_string()),
            ],
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
        }
    }

    #[tokio::test]
    async fn test_write_calls_single() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, model, is_stream, input_tokens FROM llm_calls")
            .unwrap();
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, "01912345-6789-7abc-def0-123456789abc");
        assert_eq!(row.1, "gpt-4");
        assert!(row.2);
        assert_eq!(row.3, Some(100));
    }

    #[tokio::test]
    async fn test_write_calls_new_fields() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT response_id, request_headers, response_headers FROM llm_calls")
            .unwrap();
        let (resp_id, req_hdr, resp_hdr) = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        assert_eq!(resp_id.as_deref(), Some("chatcmpl-test123"));
        // Verify headers are stored as JSON array of pairs
        let req_parsed: serde_json::Value = serde_json::from_str(&req_hdr).unwrap();
        assert!(req_parsed.is_array());
        assert_eq!(req_parsed[0][0], "authorization");
        assert_eq!(req_parsed[0][1], "Bearer sk-test");
        let resp_parsed: serde_json::Value = serde_json::from_str(&resp_hdr).unwrap();
        assert_eq!(resp_parsed[1][0], "x-request-id");
        assert_eq!(resp_parsed[1][1], "req_abc123");
    }

    #[tokio::test]
    async fn test_write_calls_id_present() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM llm_calls").unwrap();
        let id: String = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(id, "01912345-6789-7abc-def0-123456789abc");
    }

    #[tokio::test]
    async fn test_write_calls_empty_batch() {
        let backend = in_memory();
        backend.init().await.unwrap();
        backend.write_calls(vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn test_query_calls_basic() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let call = sample_call();
        let call_time = call.request_time;
        backend.write_calls(vec![call]).await.unwrap();

        let query = CallsQuery {
            time_range: TimeRange {
                start_us: call_time - 1,
                end_us: call_time + 1_000_000,
            },
            filter: DimensionFilter::default(),
            status_codes: vec![],
            finish_reasons: vec![],
            client_ips: vec![],
            server_ports: vec![],
            request_path_contains: None,
            is_stream: None,
            sort_by: "request_time".to_string(),
            sort_order: "DESC".to_string(),
            page: 1,
            page_size: 10,
        };

        let page = backend.query_calls(&query).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "01912345-6789-7abc-def0-123456789abc");
        assert_eq!(page.items[0].model, "gpt-4");
        assert_eq!(page.items[0].status_code, Some(200));
        assert_eq!(page.items[0].input_tokens, Some(100));
        assert_eq!(page.items[0].output_tokens, Some(50));
    }

    #[tokio::test]
    async fn test_query_calls_filter_status_code() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let mut call_200 = sample_call();
        call_200.id = "call-200".to_string();
        call_200.status_code = Some(200);

        let mut call_429 = sample_call();
        call_429.id = "call-429".to_string();
        call_429.status_code = Some(429);

        let call_time = call_200.request_time;
        backend.write_calls(vec![call_200, call_429]).await.unwrap();

        let query = CallsQuery {
            time_range: TimeRange {
                start_us: call_time - 1,
                end_us: call_time + 1_000_000,
            },
            filter: DimensionFilter::default(),
            status_codes: vec![429],
            finish_reasons: vec![],
            client_ips: vec![],
            server_ports: vec![],
            request_path_contains: None,
            is_stream: None,
            sort_by: "request_time".to_string(),
            sort_order: "DESC".to_string(),
            page: 1,
            page_size: 10,
        };

        let page = backend.query_calls(&query).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "call-429");
        assert_eq!(page.items[0].status_code, Some(429));
    }

    #[tokio::test]
    async fn test_query_call_by_id() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        // Query by existing id
        let detail = backend
            .query_call_by_id("01912345-6789-7abc-def0-123456789abc")
            .await
            .unwrap();
        assert!(detail.is_some());
        let detail = detail.unwrap();
        assert_eq!(detail.id, "01912345-6789-7abc-def0-123456789abc");
        assert_eq!(detail.model, "gpt-4");
        assert_eq!(detail.wire_api, wa::OPENAI_CHAT);
        assert_eq!(detail.status_code, Some(200));
        assert_eq!(detail.input_tokens, Some(100));
        assert_eq!(detail.output_tokens, Some(50));
        assert_eq!(detail.total_tokens, Some(150));
        assert!(detail.request_body.is_some());
        assert!(detail.response_body.is_some());
        assert!(detail.request_headers.is_some());
        assert!(detail.response_headers.is_some());

        // Query nonexistent id
        let not_found = backend.query_call_by_id("does-not-exist").await.unwrap();
        assert!(not_found.is_none());
    }
}
