use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use ts_storage::query::CallsQuery;
use ts_storage::StorageBackend;

use crate::extractors::{Path, Query};
use crate::params::*;
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Deserialize)]
pub struct CallsParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub status_code: Option<String>,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub client_ip: Option<String>,
    /// CSV of u16 server ports. Filters `llm_calls.server_port` directly.
    #[serde(default)]
    pub server_port: Option<String>,
    /// Substring match against `request_path` (case-sensitive, `LIKE '%…%'`).
    #[serde(default)]
    pub request_path: Option<String>,
    /// Stream-mode filter. `"stream"` / `"non-stream"` narrows the result;
    /// unset (or `"all"`) keeps every row.
    #[serde(default)]
    pub is_stream: Option<String>,
    #[serde(default = "default_calls_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_calls_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_calls_sort_by() -> String {
    "request_time".to_string()
}
fn default_calls_sort_order() -> String {
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
    Query(params): Query<CallsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let page_size = params.page_size.min(200);
    let status_codes: Vec<u16> = parse_csv(&params.status_code)
        .iter()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| ApiError::InvalidParam(format!("invalid status_code: {s}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let server_ports: Vec<u16> = parse_csv(&params.server_port)
        .iter()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| ApiError::InvalidParam(format!("invalid server_port: {s}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let is_stream = match params.is_stream.as_deref() {
        None | Some("") | Some("all") => None,
        Some("stream") | Some("true") | Some("1") => Some(true),
        Some("non-stream") | Some("nonstream") | Some("false") | Some("0") => Some(false),
        Some(other) => {
            return Err(ApiError::InvalidParam(format!(
                "is_stream must be one of: stream, non-stream, all (got '{other}')"
            )));
        }
    };

    let query = CallsQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        status_codes,
        finish_reasons: parse_csv(&params.finish_reason),
        client_ips: parse_csv(&params.client_ip),
        server_ports,
        request_path_contains: params.request_path.filter(|s| !s.is_empty()),
        is_stream,
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        page: params.page,
        page_size,
    };

    let page = storage.query_calls(&query).await?;
    Ok(ApiResponse::ok(page))
}

pub async fn detail(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match storage.query_call_by_id(&id).await? {
        Some(detail) => Ok(ApiResponse::ok(detail)),
        None => Err(ApiError::NotFound(format!("call not found: {id}"))),
    }
}
