use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use ts_llm::wire_apis::build_default_wire_api_registry;
use ts_storage::query::TurnsQuery;
use ts_storage::StorageBackend;

use super::turn_call_enrichment::enrich;
use crate::extractors::{Path, Query};
use crate::params::*;
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Deserialize)]
pub struct TurnsParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Accepted for API symmetry with calls/metrics but ignored — agent_turns
    /// does not store server_ip (turns are client-facing aggregations).
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub agent_kind: Option<String>,
    #[serde(default = "default_turns_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_turns_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_turns_sort_by() -> String {
    "start_time".to_string()
}
fn default_turns_sort_order() -> String {
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
    Query(params): Query<TurnsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let page_size = params.page_size.min(200);

    let query = TurnsQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        statuses: parse_csv(&params.status),
        agent_kinds: parse_csv(&params.agent_kind),
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        page: params.page,
        page_size,
    };

    let page = storage.query_turns(&query).await?;
    Ok(ApiResponse::ok(page))
}

pub async fn detail(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path(turn_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match storage.query_turn_by_id(&turn_id).await? {
        Some(detail) => Ok(ApiResponse::ok(detail)),
        None => Err(ApiError::NotFound(format!("turn not found: {turn_id}"))),
    }
}

pub async fn calls(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path(turn_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let items = storage.query_turn_calls(&turn_id).await?;
    let turn = storage.query_turn_by_id(&turn_id).await?;
    let final_call_id = turn.as_ref().and_then(|t| t.final_call_id.as_deref());
    let registry = build_default_wire_api_registry();
    let enriched = enrich(items, final_call_id, &registry);
    Ok(ApiResponse::ok(enriched))
}
