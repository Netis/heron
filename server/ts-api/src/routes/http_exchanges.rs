use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use ts_storage::query::HttpExchangesQuery;
use ts_storage::StorageBackend;

use crate::extractors::{Path, Query};
use crate::params::{parse_csv, to_time_range};
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Deserialize)]
pub struct HttpExchangesParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub server_ip: Option<String>,
    /// CSV of client IPs (exact match).
    #[serde(default)]
    pub client_ip: Option<String>,
    /// CSV of HTTP methods; case-insensitive (normalized to uppercase).
    #[serde(default)]
    pub method: Option<String>,
    /// CSV of integer status codes.
    #[serde(default)]
    pub status: Option<String>,
    /// Substring match against `uri` (case-sensitive, `LIKE '%…%'`).
    #[serde(default)]
    pub uri: Option<String>,
    /// `"true"` / `"false"` / absent. Absent means "any".
    #[serde(default)]
    pub is_sse: Option<bool>,
    #[serde(default = "default_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_sort_by() -> String {
    "request_time".to_string()
}
fn default_sort_order() -> String {
    "desc".to_string()
}
fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    50
}

pub async fn list(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<HttpExchangesParams>,
) -> Result<impl IntoResponse, ApiError> {
    let page_size = params.page_size.min(200);

    let methods: Vec<String> = parse_csv(&params.method)
        .into_iter()
        .map(|s| s.to_uppercase())
        .collect();

    let status_codes: Vec<u16> = parse_csv(&params.status)
        .iter()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| ApiError::InvalidParam(format!("invalid status: {s}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let query = HttpExchangesQuery {
        time_range: to_time_range(params.start, params.end)?,
        server_ips: parse_csv(&params.server_ip),
        client_ips: parse_csv(&params.client_ip),
        methods,
        status_codes,
        uri_contains: params.uri.filter(|s| !s.is_empty()),
        is_sse: params.is_sse,
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        page: params.page,
        page_size,
    };

    let page = storage.query_http_exchanges(&query).await?;
    Ok(ApiResponse::ok(page))
}

/// `GET /api/http-exchanges/:id` — fetch a single HTTP exchange row.
pub async fn detail(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match storage.query_http_exchange_by_id(&id).await? {
        Some(detail) => Ok(ApiResponse::ok(detail)),
        None => Err(ApiError::NotFound(format!("http_exchange not found: {id}"))),
    }
}
