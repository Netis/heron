use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::response::{ApiError, ApiResponse};
use crate::AppState;

#[derive(Serialize)]
struct FilterValues {
    values: Vec<String>,
}

pub async fn wire_apis(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let values = state.storage.query_distinct_wire_apis().await?;
    Ok(ApiResponse::ok(FilterValues { values }))
}

pub async fn models(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let values = state.storage.query_distinct_models().await?;
    Ok(ApiResponse::ok(FilterValues { values }))
}

pub async fn server_ips(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let values = state.storage.query_distinct_server_ips().await?;
    Ok(ApiResponse::ok(FilterValues { values }))
}
