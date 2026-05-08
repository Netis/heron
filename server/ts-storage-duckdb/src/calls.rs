//! `llm_calls` table I/O — write, query (paginated / by-id / by-id-list).

use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use ts_common::error::{AppError, Result};
use ts_llm::model::LlmCall;
use ts_storage::query::*;

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
) -> Result<Vec<TurnCallItem>> {
    if call_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", call_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
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
            request_body, response_body,
            request_headers, response_headers
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
                 client_ip, server_ip, server_port, request_path, response_body \
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
                    request_headers, response_headers
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

    pub(crate) async fn query_turn_calls(&self, turn_id: &str) -> Result<Vec<TurnCallItem>> {
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
            read_calls_by_ids_sync(&conn, &call_ids)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_calls_by_ids(
        &self,
        call_ids: &[String],
    ) -> Result<Vec<TurnCallItem>> {
        if call_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read_pool.acquire().await?;
        let call_ids: Vec<String> = call_ids.to_vec();
        tokio::task::spawn_blocking(move || read_calls_by_ids_sync(&conn, &call_ids))
            .await
            .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
}
