use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use ts_storage::query::{
    decode_session_cursor, SessionListQuery, SessionTurnsQuery, TimeRange,
};
use ts_storage::StorageBackend;

use crate::extractors::{Path, Query};
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Deserialize)]
pub struct SessionsParams {
    /// Inclusive lower bound, seconds since epoch.
    pub start: i64,
    /// Exclusive upper bound, seconds since epoch.
    pub end: i64,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub agent_kind: Option<String>,
    /// Opaque cursor from the previous page's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

#[derive(Debug, Deserialize)]
pub struct SessionTurnsParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    50
}

pub async fn list(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<SessionsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let cursor = match &params.cursor {
        Some(s) if !s.is_empty() => Some(decode_session_cursor(s).ok_or_else(|| {
            ApiError::InvalidParam("invalid cursor".to_string())
        })?),
        _ => None,
    };

    let query = SessionListQuery {
        time_range: TimeRange {
            start_us: params.start * 1_000_000,
            end_us: params.end * 1_000_000,
        },
        source_id: params.source_id.filter(|s| !s.is_empty()),
        agent_kind: params.agent_kind.filter(|s| !s.is_empty()),
        cursor,
        page_size: params.page_size.clamp(1, 200),
    };

    let page = storage.query_sessions(&query).await?;
    Ok(ApiResponse::ok(page))
}

pub async fn detail(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path((source_id, session_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    match storage.query_session_by_id(&source_id, &session_id).await? {
        Some(detail) => Ok(ApiResponse::ok(detail)),
        None => Err(ApiError::NotFound(format!(
            "session not found: {source_id}/{session_id}"
        ))),
    }
}

pub async fn turns(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path((source_id, session_id)): Path<(String, String)>,
    Query(params): Query<SessionTurnsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = SessionTurnsQuery {
        source_id,
        session_id,
        page: params.page.max(1),
        page_size: params.page_size.clamp(1, 200),
    };
    let page = storage.query_session_turns(&query).await?;
    Ok(ApiResponse::ok(page))
}
