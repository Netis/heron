//! `http_exchanges` table I/O — write + by-id / paginated reads.

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::Result;
use h_protocol::HttpExchange;
use h_storage::dialect::sql_in_list;
use h_storage::query::*;

use crate::client::insert_all;
use crate::rows::ExchangeRow;
use crate::sql::{escape_str, time_where};
use crate::ClickHouseBackend;

/// Valid `sort_by` fields for `query_http_exchanges`, mirroring the DuckDB
/// backend. `duration_ms` is a derived expression; the others are plain
/// columns (`status` is the per-exchange status column).
const VALID_SORT_FIELDS: &[&str] = &["request_time", "status", "duration_ms"];

#[derive(Row, Deserialize)]
struct CountRow {
    n: u64,
}

/// Row shape for the by-id `query_http_exchange_by_id` detail SELECT. Field
/// names + order match the SELECT column aliases / order exactly so the
/// RowBinary validator is satisfied. Timestamps are read as microseconds via
/// `toUnixTimestamp64Micro`, matching `HttpExchangeDetail`'s µs i64 fields.
#[derive(Row, Deserialize)]
struct ExchangeDetailRow {
    id: String,
    source_id: String,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    method: String,
    uri: String,
    request_headers: String,
    request_body: Option<String>,
    status: Option<u16>,
    response_headers: String,
    response_body: Option<String>,
    is_sse: bool,
    sse_event_count: u32,
    sse_data_bytes: u64,
    request_time_us: i64,
    response_first_byte_time_us: Option<i64>,
    response_complete_time_us: Option<i64>,
}

/// Row shape for the paginated `query_http_exchanges` list SELECT. Mirrors the
/// DuckDB SELECT: `request_time` is milliseconds (`epoch_ms` → `toUnixTimestamp64Milli`),
/// and `duration_ms` is the derived complete−request gap in ms (NULL when incomplete).
#[derive(Row, Deserialize)]
struct ExchangeListRow {
    id: String,
    source_id: String,
    request_time_ms: i64,
    method: String,
    uri: String,
    client_ip: String,
    server_ip: String,
    server_port: u16,
    status: Option<u16>,
    is_sse: bool,
    duration_ms: Option<f64>,
}

impl ClickHouseBackend {
    pub(crate) async fn write_exchanges(&self, exchanges: Vec<HttpExchange>) -> Result<()> {
        let rows: Vec<ExchangeRow> = exchanges.into_iter().map(ExchangeRow::from).collect();
        insert_all!(self.client, "http_exchanges", ExchangeRow, rows);
        Ok(())
    }

    pub(crate) async fn query_http_exchange_by_id(
        &self,
        id: &str,
    ) -> Result<Option<HttpExchangeDetail>> {
        let sql = format!(
            "SELECT id, source_id, \
             client_ip, client_port, server_ip, server_port, \
             method, uri, \
             request_headers, request_body, \
             status, response_headers, response_body, is_sse, \
             sse_event_count, sse_data_bytes, \
             toUnixTimestamp64Micro(request_time) AS request_time_us, \
             toUnixTimestamp64Micro(response_first_byte_time) AS response_first_byte_time_us, \
             toUnixTimestamp64Micro(response_complete_time) AS response_complete_time_us \
             FROM http_exchanges WHERE id = '{}' LIMIT 1",
            escape_str(id)
        );
        let row = self
            .client
            .query(&sql)
            .fetch_all::<ExchangeDetailRow>()
            .await
            .map_err(|e| crate::client::ch_err("query_http_exchange_by_id", e))?
            .into_iter()
            .next();
        Ok(row.map(exchange_detail))
    }

    pub(crate) async fn query_http_exchanges(
        &self,
        query: &HttpExchangesQuery,
    ) -> Result<HttpExchangesPage> {
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

        // Time-range + filter WHERE. Timestamps and numeric values are
        // interpolated (values we control); user string lists go through
        // `sql_in_list`'s single-quote escaping, valid in ClickHouse.
        let mut where_parts = vec![time_where(
            "request_time",
            query.time_range.start_us,
            query.time_range.end_us,
        )];
        if !query.server_ips.is_empty() {
            where_parts.push(format!("server_ip IN ({})", sql_in_list(&query.server_ips)));
        }
        if !query.client_ips.is_empty() {
            where_parts.push(format!("client_ip IN ({})", sql_in_list(&query.client_ips)));
        }
        if !query.methods.is_empty() {
            where_parts.push(format!("method IN ({})", sql_in_list(&query.methods)));
        }
        if !query.status_codes.is_empty() {
            where_parts.push(format!("status IN ({})", join_nums(&query.status_codes)));
        }
        if let Some(substr) = query
            .uri_contains
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            where_parts.push(format!("uri LIKE '%{}%'", escape_str(substr)));
        }
        if let Some(sse) = query.is_sse {
            where_parts.push(format!("is_sse = {}", if sse { 1 } else { 0 }));
        }
        let where_sql = where_parts.join(" AND ");

        // Map virtual field → column/expression for ORDER BY. `duration_ms`
        // and `status` get `NULLS LAST` so incomplete (duration/status=None)
        // rows don't dominate a descending sort, matching the DuckDB backend.
        let order_expr = match query.sort_by.as_str() {
            "duration_ms" => {
                "(toUnixTimestamp64Micro(response_complete_time) \
                 - toUnixTimestamp64Micro(request_time)) / 1000.0 NULLS LAST"
            }
            "status" => "status NULLS LAST",
            _ => "request_time",
        };

        let total = self
            .client
            .query(&format!(
                "SELECT count() AS n FROM http_exchanges WHERE {where_sql}"
            ))
            .fetch_one::<CountRow>()
            .await
            .map_err(|e| crate::client::ch_err("query_http_exchanges count", e))?
            .n;

        let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
        // `request_time` as ms (epoch_ms) and `duration_ms` as the complete−request
        // gap in ms (NULL when incomplete), mirroring the DuckDB list SELECT.
        let items_sql = format!(
            "SELECT id, source_id, \
             toUnixTimestamp64Milli(request_time) AS request_time_ms, \
             method, uri, client_ip, server_ip, server_port, \
             status, is_sse, \
             CASE WHEN response_complete_time IS NOT NULL \
                  THEN (toUnixTimestamp64Micro(response_complete_time) \
                        - toUnixTimestamp64Micro(request_time)) / 1000.0 \
                  ELSE NULL END AS duration_ms \
             FROM http_exchanges WHERE {where_sql} \
             ORDER BY {order_expr} {sort_order} \
             LIMIT {} OFFSET {offset}",
            query.page_size,
        );
        let rows = self
            .client
            .query(&items_sql)
            .fetch_all::<ExchangeListRow>()
            .await
            .map_err(|e| crate::client::ch_err("query_http_exchanges items", e))?;

        let items = rows.into_iter().map(exchange_list_item).collect();
        Ok(HttpExchangesPage { total, items })
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

fn exchange_detail(r: ExchangeDetailRow) -> HttpExchangeDetail {
    HttpExchangeDetail {
        id: r.id,
        source_id: r.source_id,
        client_ip: r.client_ip,
        client_port: r.client_port,
        server_ip: r.server_ip,
        server_port: r.server_port,
        method: r.method,
        uri: r.uri,
        request_headers: r.request_headers,
        // Bodies are already `String` in ClickHouse (DuckDB stores BLOB and
        // renders to UTF-8 via `render_body_for_detail`); pass through.
        request_body: r.request_body,
        status: r.status,
        response_headers: r.response_headers,
        response_body: r.response_body,
        is_sse: r.is_sse,
        sse_event_count: r.sse_event_count,
        sse_data_bytes: r.sse_data_bytes,
        request_time: r.request_time_us,
        response_first_byte_time: r.response_first_byte_time_us,
        response_complete_time: r.response_complete_time_us,
    }
}

fn exchange_list_item(r: ExchangeListRow) -> HttpExchangeListItem {
    HttpExchangeListItem {
        id: r.id,
        source_id: r.source_id,
        request_time: r.request_time_ms,
        method: r.method,
        uri: r.uri,
        client_ip: r.client_ip,
        server_ip: r.server_ip,
        server_port: r.server_port,
        status: r.status,
        is_sse: r.is_sse,
        duration_ms: r.duration_ms,
    }
}
