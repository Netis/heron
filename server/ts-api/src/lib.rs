//! HTTP API server — read-only query layer over the storage backend.
//!
//! Not part of the ingest pipeline. Runs alongside it in the same process,
//! receives an [`AppState`] bundling the storage backend and the live
//! capture [`SourceRegistry`] (for `/api/sources`) to serve queries
//! against `llm_calls` / `agent_turns` / `llm_metrics` and to surface
//! which capture sources are currently sending data.
//!
//! Entry points:
//!
//! * [`bind`] — bind the TCP listener (fail-fast before pipeline spawn)
//! * [`router`] — build the Axum `Router` (useful for composition / tests)
//! * [`serve`] — run the server on an already-bound listener

pub mod extractors;
pub mod params;
pub mod response;
pub mod routes;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use ts_common::config::ApiConfig;
use ts_common::error::{AppError, Result};
use ts_common::source_registry::SourceRegistry;
use ts_storage::StorageBackend;

/// Shared read-only state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<dyn StorageBackend>,
    pub sources: Arc<SourceRegistry>,
}

/// Bind the API server listener. Call this before spawning so bind errors
/// propagate to the caller (and can abort startup).
pub async fn bind(config: &ApiConfig) -> Result<TcpListener> {
    let addr = format!("{}:{}", config.listen, config.port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| AppError::Config(format!("failed to bind API server to {addr}: {e}")))?;
    tracing::info!("API server listening on {addr}");
    Ok(listener)
}

/// Build the API router (without serving). Useful for composing with other layers.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/filters/wire_apis", get(routes::filters::wire_apis))
        .route("/api/filters/models", get(routes::filters::models))
        .route("/api/filters/server_ips", get(routes::filters::server_ips))
        .route("/api/metrics/timeseries", get(routes::metrics::timeseries))
        .route("/api/metrics/summary", get(routes::metrics::summary))
        .route("/api/metrics/models", get(routes::metrics::models))
        .route("/api/calls", get(routes::calls::list))
        .route("/api/calls/{id}", get(routes::calls::detail))
        .route("/api/turns", get(routes::turns::list))
        .route("/api/turns/{id}", get(routes::turns::detail))
        .route("/api/turns/{id}/calls", get(routes::turns::calls))
        .route("/api/sources", get(routes::sources::list))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Serve the API on an already-bound listener (runs until shutdown).
pub async fn serve(listener: TcpListener, state: AppState) -> Result<()> {
    let app = router(state);
    axum::serve(listener, app).await.map_err(AppError::Io)?;
    Ok(())
}
