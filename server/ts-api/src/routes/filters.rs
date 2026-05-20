use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use ts_storage::query::{DistinctAgentKindsQuery, DistinctFinishReason};
use ts_storage::StorageBackend;

use crate::extractors::Query;
use crate::params::{to_dimension_filter, to_time_range};
use crate::response::{ApiError, ApiResponse};

#[derive(Serialize)]
struct FilterValues {
    values: Vec<String>,
}

#[derive(Serialize)]
struct FinishReasonPairs {
    pairs: Vec<DistinctFinishReason>,
}

#[derive(Debug, Deserialize)]
pub struct AgentKindsParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub include_proxy_hops: bool,
}

pub async fn wire_apis(
    State(storage): State<Arc<dyn StorageBackend>>,
) -> Result<impl IntoResponse, ApiError> {
    let values = storage.query_distinct_wire_apis().await?;
    Ok(ApiResponse::ok(FilterValues { values }))
}

pub async fn models(
    State(storage): State<Arc<dyn StorageBackend>>,
) -> Result<impl IntoResponse, ApiError> {
    let values = storage.query_distinct_models().await?;
    Ok(ApiResponse::ok(FilterValues { values }))
}

pub async fn server_ips(
    State(storage): State<Arc<dyn StorageBackend>>,
) -> Result<impl IntoResponse, ApiError> {
    let values = storage.query_distinct_server_ips().await?;
    Ok(ApiResponse::ok(FilterValues { values }))
}

pub async fn agent_kinds(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<AgentKindsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = DistinctAgentKindsQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        include_proxy_hops: params.include_proxy_hops,
    };
    let values = storage.query_distinct_agent_kinds(&query).await?;
    Ok(ApiResponse::ok(FilterValues { values }))
}

pub async fn finish_reasons(
    State(storage): State<Arc<dyn StorageBackend>>,
) -> Result<impl IntoResponse, ApiError> {
    let pairs = storage.query_distinct_finish_reasons().await?;
    Ok(ApiResponse::ok(FinishReasonPairs { pairs }))
}
