//! `http_exchanges` table I/O — write + by-id / paginated queries.

use duckdb::types::{TimeUnit, Value};
use ts_common::error::{AppError, Result};
use ts_protocol::HttpExchange;
use ts_storage::query::*;

use crate::util::{headers_to_json, render_body_for_detail, us_to_timestamp};
use crate::DuckDbBackend;

struct PreparedExchange {
    id: String,
    source_id: String,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    method: String,
    uri: String,
    request_headers: String,
    request_body: Option<Vec<u8>>,
    status: Option<u16>,
    response_headers: String,
    response_body: Option<Vec<u8>>,
    is_sse: bool,
    sse_event_count: u32,
    sse_data_bytes: u64,
    request_time: Value,
    response_first_byte_time: Option<Value>,
    response_complete_time: Option<Value>,
}

fn prepare_exchange(x: HttpExchange) -> PreparedExchange {
    let (client_ip, client_port) = x.client_addr();
    let (server_ip, server_port) = x.server_addr();
    let is_sse = x.is_sse();
    let stored_response_body = x.stored_response_body().map(|b| b.to_vec());
    let request_body = if x.request.body.is_empty() {
        None
    } else {
        Some(x.request.body.to_vec())
    };
    PreparedExchange {
        id: x.id,
        source_id: x.request.flow_key.source_id.clone(),
        client_ip: client_ip.to_string(),
        client_port,
        server_ip: server_ip.to_string(),
        server_port,
        method: x.request.method.clone(),
        uri: x.request.uri.clone(),
        request_headers: headers_to_json(&x.request.headers),
        request_body,
        status: Some(x.response.status),
        response_headers: headers_to_json(&x.response.headers),
        response_body: stored_response_body,
        is_sse,
        sse_event_count: x.sse_event_count,
        sse_data_bytes: x.sse_data_bytes,
        request_time: Value::Timestamp(TimeUnit::Microsecond, x.request.timestamp_us),
        response_first_byte_time: Some(Value::Timestamp(
            TimeUnit::Microsecond,
            x.response.first_byte_timestamp_us,
        )),
        response_complete_time: Some(Value::Timestamp(
            TimeUnit::Microsecond,
            x.response.complete_timestamp_us,
        )),
    }
}

impl DuckDbBackend {
    pub(crate) async fn write_exchanges(&self, exchanges: Vec<HttpExchange>) -> Result<()> {
        if exchanges.is_empty() {
            return Ok(());
        }
        let conn = self.write_exchanges_conn.clone();
        tokio::task::spawn_blocking(move || {
            let prepared: Vec<PreparedExchange> =
                exchanges.into_iter().map(prepare_exchange).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("http_exchanges")
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
                        p.method,
                        p.uri,
                        p.request_headers,
                        p.request_body,
                        p.status,
                        p.response_headers,
                        p.response_body,
                        p.is_sse,
                        p.sse_event_count,
                        p.sse_data_bytes,
                        p.request_time,
                        p.response_first_byte_time,
                        p.response_complete_time,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append exchange: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush exchanges: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_http_exchange_by_id(
        &self,
        id: &str,
    ) -> Result<Option<HttpExchangeDetail>> {
        let conn = self.read_pool.acquire().await?;
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let sql = "
                SELECT id, source_id,
                    client_ip, client_port, server_ip, server_port,
                    method, uri,
                    request_headers, request_body,
                    status, response_headers, response_body, is_sse,
                    sse_event_count, sse_data_bytes,
                    epoch_ms(request_time),
                    epoch_ms(response_first_byte_time),
                    epoch_ms(response_complete_time)
                FROM http_exchanges
                WHERE id = ?
            ";
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare exchange query: {e}")))?;
            let result = stmt.query_row(duckdb::params![id], |row| {
                let request_body_bytes: Option<Vec<u8>> = row.get(9)?;
                let response_body_bytes: Option<Vec<u8>> = row.get(12)?;
                Ok(HttpExchangeDetail {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    client_ip: row.get(2)?,
                    client_port: row.get(3)?,
                    server_ip: row.get(4)?,
                    server_port: row.get(5)?,
                    method: row.get(6)?,
                    uri: row.get(7)?,
                    request_headers: row.get(8)?,
                    request_body: render_body_for_detail(request_body_bytes),
                    status: row.get(10)?,
                    response_headers: row.get(11)?,
                    response_body: render_body_for_detail(response_body_bytes),
                    is_sse: row.get(13)?,
                    sse_event_count: row.get(14)?,
                    sse_data_bytes: row.get(15)?,
                    request_time: row.get(16)?,
                    response_first_byte_time: row.get(17)?,
                    response_complete_time: row.get(18)?,
                })
            });
            match result {
                Ok(d) => Ok(Some(d)),
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(AppError::Storage(format!(
                    "failed to query http_exchange by id: {e}"
                ))),
            }
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_http_exchanges(
        &self,
        query: &HttpExchangesQuery,
    ) -> Result<HttpExchangesPage> {
        // `duration_ms` is a derived expression; the others are plain columns.
        const VALID_SORT_FIELDS: &[&str] = &["request_time", "status", "duration_ms"];
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

            let mut where_parts = vec![
                "request_time >= ?".to_string(),
                "request_time < ?".to_string(),
            ];
            if !query.server_ips.is_empty() {
                let list: Vec<String> = query
                    .server_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("server_ip IN ({})", list.join(", ")));
            }
            if !query.client_ips.is_empty() {
                let list: Vec<String> = query
                    .client_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("client_ip IN ({})", list.join(", ")));
            }
            if !query.methods.is_empty() {
                let list: Vec<String> = query
                    .methods
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("method IN ({})", list.join(", ")));
            }
            if !query.status_codes.is_empty() {
                let list: Vec<String> = query.status_codes.iter().map(|c| c.to_string()).collect();
                where_parts.push(format!("status IN ({})", list.join(", ")));
            }
            if let Some(substr) = query
                .uri_contains
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                where_parts.push(format!("uri LIKE '%{}%'", substr.replace('\'', "''")));
            }
            if let Some(sse) = query.is_sse {
                where_parts.push(format!("is_sse = {sse}"));
            }

            let where_sql = where_parts.join(" AND ");
            // Map virtual field → column/expression for ORDER BY. `duration_ms`
            // becomes an expression; plain columns pass through. NULLS LAST
            // keeps incomplete (duration/status=None) rows from dominating
            // descending sort.
            let order_expr = match query.sort_by.as_str() {
                "duration_ms" => "epoch_ms(response_complete_time - request_time) NULLS LAST",
                "status" => "status NULLS LAST",
                _ => "request_time",
            };

            // COUNT
            let count_sql = format!("SELECT COUNT(*) FROM http_exchanges WHERE {where_sql}");
            let mut count_stmt = conn
                .prepare(&count_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare count query: {e}")))?;
            let total: u64 = count_stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| row.get(0))
                .map_err(|e| AppError::Storage(format!("failed to execute count query: {e}")))?;

            // Items
            let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
            let limit = query.page_size;
            let items_sql = format!(
                "SELECT id, source_id, epoch_ms(request_time), \
                 method, uri, client_ip, server_ip, server_port, \
                 status, is_sse, \
                 CASE WHEN response_complete_time IS NOT NULL \
                      THEN epoch_ms(response_complete_time - request_time) \
                      ELSE NULL END AS duration_ms \
                 FROM http_exchanges WHERE {where_sql} \
                 ORDER BY {order_expr} {sort_order} \
                 LIMIT {limit} OFFSET {offset}"
            );
            let mut items_stmt = conn
                .prepare(&items_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare items query: {e}")))?;

            let mut items = Vec::new();
            let mut rows = items_stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute items query: {e}")))?;
            while let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                items.push(HttpExchangeListItem {
                    id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    source_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_time: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    method: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    uri: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    client_ip: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_ip: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_port: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    status: row
                        .get::<_, Option<u16>>(8)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    is_sse: row
                        .get(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    duration_ms: row
                        .get::<_, Option<f64>>(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                });
            }
            Ok(HttpExchangesPage { total, items })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use std::net::IpAddr;
    use ts_storage::StorageBackend;

    fn in_memory() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
    }

    #[tokio::test]
    async fn http_exchange_round_trip() {
        use bytes::Bytes;
        use std::sync::Arc;
        use ts_protocol::model::{HttpRequestData, HttpResponseData};
        use ts_protocol::net::FlowKey;
        let backend = in_memory();
        backend.init().await.unwrap();
        let client_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let server_ip: IpAddr = "10.0.0.2".parse().unwrap();
        let request = Arc::new(HttpRequestData {
            flow_key: FlowKey::new("source-x".into(), client_ip, 54321, server_ip, 443),
            client_addr: (client_ip, 54321),
            server_addr: (server_ip, 443),
            method: "POST".into(),
            uri: "/v1/chat/completions".into(),
            version: 1,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from_static(br#"{"model":"gpt-4"}"#),
            timestamp_us: 1_700_000_000_000_000,
        });
        let response = Arc::new(HttpResponseData {
            flow_key: request.flow_key.clone(),
            client_addr: request.client_addr,
            server_addr: request.server_addr,
            status: 200,
            version: 1,
            headers: vec![("x-request-id".into(), "req_abc".into())],
            body: Bytes::from_static(br#"{"choices":[]}"#),
            first_byte_timestamp_us: 1_700_000_000_500_000,
            complete_timestamp_us: 1_700_000_001_000_000,
        });
        let exchange = ts_protocol::HttpExchange {
            id: "xchg-rt-1".to_string(),
            request,
            response,
            sse_event_count: 0,
            sse_data_bytes: 0,
        };
        backend
            .write_exchanges(vec![exchange.clone()])
            .await
            .unwrap();
        let got = backend
            .query_http_exchange_by_id("xchg-rt-1")
            .await
            .unwrap()
            .expect("round-tripped exchange");
        assert_eq!(got.id, "xchg-rt-1");
        assert_eq!(got.client_port, 54321);
        assert_eq!(got.method, "POST");
        assert_eq!(got.status, Some(200));
        assert!(!got.is_sse);
        assert_eq!(got.request_body.as_deref(), Some(r#"{"model":"gpt-4"}"#));
        assert_eq!(got.response_body.as_deref(), Some(r#"{"choices":[]}"#));
    }

    #[tokio::test]
    async fn http_exchange_sse_round_trip_response_body_none() {
        use bytes::Bytes;
        use std::sync::Arc;
        use ts_protocol::model::{HttpRequestData, HttpResponseData};
        use ts_protocol::net::FlowKey;
        let backend = in_memory();
        backend.init().await.unwrap();
        let client_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let server_ip: IpAddr = "10.0.0.2".parse().unwrap();
        let request = Arc::new(HttpRequestData {
            flow_key: FlowKey::new("source-sse".into(), client_ip, 1, server_ip, 443),
            client_addr: (client_ip, 1),
            server_addr: (server_ip, 443),
            method: "POST".into(),
            uri: "/v1/messages".into(),
            version: 1,
            headers: vec![],
            body: Bytes::new(),
            timestamp_us: 1,
        });
        let response = Arc::new(HttpResponseData {
            flow_key: request.flow_key.clone(),
            client_addr: request.client_addr,
            server_addr: request.server_addr,
            status: 200,
            version: 1,
            // text/event-stream content-type drives is_sse() = true, which
            // makes `stored_response_body()` return None regardless of the
            // parser-emitted empty `body`.
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: Bytes::new(),
            first_byte_timestamp_us: 2,
            complete_timestamp_us: 3,
        });
        let exchange = ts_protocol::HttpExchange {
            id: "xchg-sse-1".to_string(),
            request,
            response,
            sse_event_count: 3,
            sse_data_bytes: 42,
        };
        backend.write_exchanges(vec![exchange]).await.unwrap();
        let got = backend
            .query_http_exchange_by_id("xchg-sse-1")
            .await
            .unwrap()
            .unwrap();
        assert!(got.is_sse);
        assert!(got.response_body.is_none());
        assert_eq!(got.sse_event_count, 3);
        assert_eq!(got.sse_data_bytes, 42);
    }

    #[tokio::test]
    async fn http_exchange_missing_id_returns_none() {
        let backend = in_memory();
        backend.init().await.unwrap();
        let got = backend.query_http_exchange_by_id("nope").await.unwrap();
        assert!(got.is_none());
    }
}
