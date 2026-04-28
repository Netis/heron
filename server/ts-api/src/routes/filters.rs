use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use ts_storage::query::DistinctFinishReason;
use ts_storage::StorageBackend;

use crate::response::{ApiError, ApiResponse};

#[derive(Serialize)]
struct FilterValues {
    values: Vec<String>,
}

#[derive(Serialize)]
struct FinishReasonPairs {
    pairs: Vec<DistinctFinishReason>,
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

pub async fn finish_reasons(
    State(storage): State<Arc<dyn StorageBackend>>,
) -> Result<impl IntoResponse, ApiError> {
    let pairs = storage.query_distinct_finish_reasons().await?;
    Ok(ApiResponse::ok(FinishReasonPairs { pairs }))
}
