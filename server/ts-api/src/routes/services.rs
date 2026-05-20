//! `GET /api/services` — Services view.
//!
//! One row per unique `(server_ip, server_port)` endpoint with the
//! models that endpoint served, error counts, throughput, and TTFT /
//! E2E percentiles in the requested window. Powers the Console's
//! Services page.
//!
//! Backed by `llm_calls` (NOT the pre-aggregated `llm_metrics` table
//! — that schema's grouping sets stop at `server_ip`, which would
//! conflate multiple LLM servers on the same host).

use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ts_storage::query::ServicesQuery;
use ts_storage::StorageBackend;

use crate::extractors::Query;
use crate::params::to_time_range;
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Deserialize)]
pub struct ServicesParams {
    /// Inclusive start in seconds since epoch.
    pub start: i64,
    /// Exclusive end in seconds since epoch.
    pub end: i64,
    #[serde(default = "default_services_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_services_limit")]
    pub limit: u32,
}

fn default_services_sort_by() -> String {
    "call_count".to_string()
}

fn default_sort_order() -> String {
    "desc".to_string()
}

fn default_services_limit() -> u32 {
    // Soft cap — even a busy install rarely has more than a couple
    // dozen distinct LLM serving endpoints. 200 is a sanity ceiling
    // so a misconfigured deployment can't return a giant payload.
    200
}

#[derive(Serialize)]
struct ServicesData {
    services: Vec<ts_storage::query::ServiceRow>,
}

pub async fn services(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<ServicesParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = ServicesQuery {
        time_range: to_time_range(params.start, params.end),
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        limit: params.limit.min(500),
    };
    let rows = storage.query_services(&query).await?;
    Ok(ApiResponse::ok(ServicesData { services: rows }))
}
