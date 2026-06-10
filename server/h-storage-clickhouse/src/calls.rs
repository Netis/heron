//! `llm_calls` table I/O — write + paginated / by-id / by-id-list reads.

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::Result;
use h_common::process::ProcessInfo;
use h_llm::model::LlmCall;
use h_storage::convert::{derive_tokens_estimated, parse_json_string_list};
use h_storage::dialect::sql_in_list;
use h_storage::query::*;

use crate::client::{ch_err, insert_all};
use crate::rows::CallRow;
use crate::sql::{escape_str, time_where};
use crate::ClickHouseBackend;

const VALID_SORT_FIELDS: &[&str] = &[
    "request_time",
    "status_code",
    "ttft_ms",
    "e2e_latency_ms",
    "input_tokens",
    "output_tokens",
];

#[derive(Row, Deserialize)]
struct CountRow {
    n: u64,
}

/// Row shape for the paginated `query_calls` list (mirrors the DuckDB SELECT).
#[derive(Row, Deserialize)]
struct CallListRow {
    id: String,
    source_id: String,
    request_time_ms: i64,
    wire_api: String,
    model: String,
    status_code: Option<u16>,
    is_stream: bool,
    finish_reason: Option<String>,
    ttft_ms: Option<f64>,
    e2e_latency_ms: Option<f64>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    client_ip: String,
    server_ip: String,
    server_port: u16,
    request_path: String,
    response_body: Option<String>,
    is_agent_request: bool,
    tool_surface: Option<String>,
    agent_topology: Option<String>,
    tool_call_count: u32,
    tool_names_json: Option<String>,
    process_pid: Option<u32>,
    process_comm: Option<String>,
    process_exe: Option<String>,
}

#[derive(Row, Deserialize)]
struct CallDetailRow {
    id: String,
    source_id: String,
    request_time_ms: i64,
    response_time_ms: Option<i64>,
    complete_time_ms: Option<i64>,
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
    ttft_ms: Option<f64>,
    e2e_latency_ms: Option<f64>,
    response_id: Option<String>,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    request_body: Option<String>,
    response_body: Option<String>,
    request_headers: String,
    response_headers: String,
    is_agent_request: bool,
    tool_surface: Option<String>,
    agent_topology: Option<String>,
    tool_call_count: u32,
    tool_names_json: Option<String>,
    process_pid: Option<u32>,
    process_comm: Option<String>,
    process_exe: Option<String>,
}

#[derive(Row, Deserialize)]
struct TurnCallRow {
    id: String,
    request_time_ms: i64,
    response_time_ms: Option<i64>,
    complete_time_ms: Option<i64>,
    wire_api: String,
    model: String,
    status_code: Option<u16>,
    is_stream: bool,
    finish_reason: Option<String>,
    ttft_ms: Option<f64>,
    e2e_latency_ms: Option<f64>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    request_path: String,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    request_body: Option<String>,
    response_body: Option<String>,
    request_headers: Option<String>,
    response_headers: Option<String>,
}

impl ClickHouseBackend {
    pub(crate) async fn write_calls(&self, calls: Vec<LlmCall>) -> Result<()> {
        let rows: Vec<CallRow> = calls.into_iter().map(CallRow::from).collect();
        insert_all!(self.client, "llm_calls", CallRow, rows);
        Ok(())
    }

    pub(crate) async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage> {
        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(h_common::error::AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.eq_ignore_ascii_case("ASC") {
            "ASC"
        } else {
            "DESC"
        };

        // Time-range + filter WHERE. Timestamps and numeric ports are
        // interpolated (values we control); user string lists go through
        // `sql_in_list`'s single-quote escaping, valid in ClickHouse.
        let mut where_parts =
            vec![time_where("request_time", query.time_range.start_us, query.time_range.end_us)];
        if !query.filter.wire_apis.is_empty() {
            where_parts.push(format!("wire_api IN ({})", sql_in_list(&query.filter.wire_apis)));
        }
        if !query.filter.models.is_empty() {
            where_parts.push(format!("model IN ({})", sql_in_list(&query.filter.models)));
        }
        if !query.filter.server_ips.is_empty() {
            where_parts.push(format!("server_ip IN ({})", sql_in_list(&query.filter.server_ips)));
        }
        if !query.status_codes.is_empty() {
            where_parts.push(format!("status_code IN ({})", join_nums(&query.status_codes)));
        }
        if !query.finish_reasons.is_empty() {
            where_parts.push(format!(
                "finish_reason IN ({})",
                sql_in_list(&query.finish_reasons)
            ));
        }
        if !query.client_ips.is_empty() {
            where_parts.push(format!("client_ip IN ({})", sql_in_list(&query.client_ips)));
        }
        if !query.server_ports.is_empty() {
            where_parts.push(format!("server_port IN ({})", join_nums(&query.server_ports)));
        }
        if let Some(substr) = query
            .request_path_contains
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            where_parts.push(format!("request_path LIKE '%{}%'", escape_str(substr)));
        }
        if let Some(stream) = query.is_stream {
            where_parts.push(format!("is_stream = {}", if stream { 1 } else { 0 }));
        }
        let where_sql = where_parts.join(" AND ");

        let total = self
            .client
            .query(&format!("SELECT count() AS n FROM llm_calls WHERE {where_sql}"))
            .fetch_one::<CountRow>()
            .await
            .map_err(|e| ch_err("query_calls count", e))?
            .n;

        let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
        let items_sql = format!(
            "SELECT id, source_id, toUnixTimestamp64Milli(request_time) AS request_time_ms, \
             wire_api, model, status_code, is_stream, finish_reason, ttft_ms, e2e_latency_ms, \
             input_tokens, output_tokens, client_ip, server_ip, server_port, request_path, \
             response_body, is_agent_request, tool_surface, agent_topology, tool_call_count, \
             tool_names_json, process_pid, process_comm, process_exe \
             FROM llm_calls WHERE {where_sql} \
             ORDER BY {} {sort_order} LIMIT {} OFFSET {offset}",
            query.sort_by, query.page_size,
        );
        let rows = self
            .client
            .query(&items_sql)
            .fetch_all::<CallListRow>()
            .await
            .map_err(|e| ch_err("query_calls items", e))?;

        let items = rows.into_iter().map(call_list_item).collect();
        Ok(CallsPage { total, items })
    }

    pub(crate) async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>> {
        let sql = format!(
            "SELECT id, source_id, \
             toUnixTimestamp64Milli(request_time) AS request_time_ms, \
             toUnixTimestamp64Milli(response_time) AS response_time_ms, \
             toUnixTimestamp64Milli(complete_time) AS complete_time_ms, \
             wire_api, model, api_type, is_stream, request_path, status_code, finish_reason, \
             input_tokens, output_tokens, total_tokens, ttft_ms, e2e_latency_ms, response_id, \
             client_ip, client_port, server_ip, server_port, request_body, response_body, \
             request_headers, response_headers, is_agent_request, tool_surface, agent_topology, \
             tool_call_count, tool_names_json, process_pid, process_comm, process_exe \
             FROM llm_calls WHERE id = '{}' LIMIT 1",
            escape_str(id)
        );
        let row = self
            .client
            .query(&sql)
            .fetch_all::<CallDetailRow>()
            .await
            .map_err(|e| ch_err("query_call_by_id", e))?
            .into_iter()
            .next();
        Ok(row.map(call_detail))
    }

    pub(crate) async fn query_turn_calls(
        &self,
        turn_id: &str,
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        // No-JOIN two-step: resolve the turn's ordered call_ids, then fetch.
        let call_ids = self.turn_call_ids(turn_id).await?;
        self.read_calls_by_ids(&call_ids, include_bodies).await
    }

    pub(crate) async fn query_calls_by_ids(
        &self,
        call_ids: &[String],
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        self.read_calls_by_ids(call_ids, include_bodies).await
    }

    /// Read `agent_turns.call_ids` (JSON array) for one turn. `FINAL` so the
    /// latest ReplacingMergeTree version wins.
    async fn turn_call_ids(&self, turn_id: &str) -> Result<Vec<String>> {
        #[derive(Row, Deserialize)]
        struct CallIdsRow {
            call_ids: String,
        }
        let sql = format!(
            "SELECT call_ids FROM agent_turns FINAL WHERE turn_id = '{}' LIMIT 1",
            escape_str(turn_id)
        );
        let row = self
            .client
            .query(&sql)
            .fetch_all::<CallIdsRow>()
            .await
            .map_err(|e| ch_err("turn_call_ids", e))?
            .into_iter()
            .next();
        Ok(row
            .map(|r| parse_json_string_list(Some(&r.call_ids)))
            .unwrap_or_default())
    }

    /// Shared "fetch calls by id list" — used by `query_turn_calls` (ids from
    /// the persisted `agent_turns.call_ids`) and `query_calls_by_ids` (ids from
    /// the in-memory active-turn registry). Calls not yet flushed simply don't
    /// return. Lite mode (`include_bodies = false`) selects NULL for the four
    /// heavy body/header fields.
    async fn read_calls_by_ids(
        &self,
        call_ids: &[String],
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        if call_ids.is_empty() {
            return Ok(Vec::new());
        }
        let body_columns = if include_bodies {
            // request_body/response_body are Nullable; headers are non-null
            // String columns — toNullable() so the row type is uniform across
            // full + lite mode (lite selects CAST(NULL AS Nullable(String))).
            "request_body, response_body, \
             toNullable(request_headers) AS request_headers, \
             toNullable(response_headers) AS response_headers"
        } else {
            "CAST(NULL AS Nullable(String)) AS request_body, \
             CAST(NULL AS Nullable(String)) AS response_body, \
             CAST(NULL AS Nullable(String)) AS request_headers, \
             CAST(NULL AS Nullable(String)) AS response_headers"
        };
        let sql = format!(
            "SELECT id, \
             toUnixTimestamp64Milli(request_time) AS request_time_ms, \
             toUnixTimestamp64Milli(response_time) AS response_time_ms, \
             toUnixTimestamp64Milli(complete_time) AS complete_time_ms, \
             wire_api, model, status_code, is_stream, finish_reason, ttft_ms, e2e_latency_ms, \
             input_tokens, output_tokens, request_path, client_ip, client_port, server_ip, \
             server_port, {body_columns} \
             FROM llm_calls WHERE id IN ({}) \
             ORDER BY request_time ASC, complete_time ASC",
            sql_in_list(call_ids),
        );
        let rows = self
            .client
            .query(&sql)
            .fetch_all::<TurnCallRow>()
            .await
            .map_err(|e| ch_err("read_calls_by_ids", e))?;

        let items = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                let tokens_estimated = derive_tokens_estimated(
                    r.input_tokens,
                    r.output_tokens,
                    r.response_body.as_deref(),
                );
                TurnCallItem {
                    id: r.id,
                    sequence: (i as u32) + 1,
                    request_time: r.request_time_ms,
                    response_time: r.response_time_ms,
                    complete_time: r.complete_time_ms,
                    wire_api: r.wire_api,
                    model: r.model,
                    status_code: r.status_code,
                    is_stream: r.is_stream,
                    finish_reason: r.finish_reason,
                    ttft_ms: r.ttft_ms,
                    e2e_latency_ms: r.e2e_latency_ms,
                    input_tokens: r.input_tokens,
                    output_tokens: r.output_tokens,
                    tokens_estimated,
                    request_path: r.request_path,
                    client_ip: r.client_ip,
                    client_port: r.client_port,
                    server_ip: r.server_ip,
                    server_port: r.server_port,
                    request_body: r.request_body,
                    response_body: r.response_body,
                    request_headers: r.request_headers,
                    response_headers: r.response_headers,
                }
            })
            .collect();
        Ok(items)
    }
}

/// Join numeric values into a comma list for `IN (...)` (no quoting needed).
fn join_nums<T: std::fmt::Display>(values: &[T]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build `Option<ProcessInfo>` from the three nullable `process_*` columns.
/// `None` exactly when `pid` is NULL (passive-tap rows).
fn row_process(pid: Option<u32>, comm: Option<String>, exe: Option<String>) -> Option<ProcessInfo> {
    pid.map(|pid| ProcessInfo {
        pid,
        comm: comm.unwrap_or_default(),
        exe,
    })
}

fn call_list_item(r: CallListRow) -> CallListItem {
    let tokens_estimated =
        derive_tokens_estimated(r.input_tokens, r.output_tokens, r.response_body.as_deref());
    let tool_names = parse_json_string_list(r.tool_names_json.as_deref());
    let process = row_process(r.process_pid, r.process_comm, r.process_exe);
    CallListItem {
        id: r.id,
        source_id: r.source_id,
        request_time: r.request_time_ms,
        wire_api: r.wire_api,
        model: r.model,
        status_code: r.status_code,
        is_stream: r.is_stream,
        finish_reason: r.finish_reason,
        ttft_ms: r.ttft_ms,
        e2e_latency_ms: r.e2e_latency_ms,
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        tokens_estimated,
        client_ip: r.client_ip,
        server_ip: r.server_ip,
        server_port: r.server_port,
        request_path: r.request_path,
        is_agent_request: r.is_agent_request,
        tool_surface: r.tool_surface,
        agent_topology: r.agent_topology,
        tool_call_count: r.tool_call_count,
        tool_names,
        process,
    }
}

fn call_detail(r: CallDetailRow) -> CallDetail {
    let tokens_estimated =
        derive_tokens_estimated(r.input_tokens, r.output_tokens, r.response_body.as_deref());
    let tool_names = parse_json_string_list(r.tool_names_json.as_deref());
    let process = row_process(r.process_pid, r.process_comm, r.process_exe);
    CallDetail {
        id: r.id,
        source_id: r.source_id,
        request_time: r.request_time_ms,
        response_time: r.response_time_ms,
        complete_time: r.complete_time_ms,
        wire_api: r.wire_api,
        model: r.model,
        api_type: r.api_type,
        is_stream: r.is_stream,
        request_path: r.request_path,
        status_code: r.status_code,
        finish_reason: r.finish_reason,
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        total_tokens: r.total_tokens,
        tokens_estimated,
        ttft_ms: r.ttft_ms,
        e2e_latency_ms: r.e2e_latency_ms,
        response_id: r.response_id,
        client_ip: r.client_ip,
        client_port: r.client_port,
        server_ip: r.server_ip,
        server_port: r.server_port,
        request_body: r.request_body,
        response_body: r.response_body,
        request_headers: Some(r.request_headers),
        response_headers: Some(r.response_headers),
        is_agent_request: r.is_agent_request,
        tool_surface: r.tool_surface,
        agent_topology: r.agent_topology,
        tool_call_count: r.tool_call_count,
        tool_names,
        process,
    }
}
