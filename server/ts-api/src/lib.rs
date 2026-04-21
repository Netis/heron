//! HTTP API server — read-only query layer over the storage backend.
//!
//! Not part of the ingest pipeline. Runs alongside it in the same process,
//! receives an `Arc<dyn StorageBackend>` to serve queries against
//! `llm_calls` / `agent_turns` / `llm_metrics`.
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
use ts_storage::StorageBackend;

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
pub fn router(storage: Arc<dyn StorageBackend>) -> Router {
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
        .layer(CorsLayer::permissive())
        .with_state(storage)
}

/// Serve the API on an already-bound listener (runs until shutdown).
pub async fn serve(listener: TcpListener, storage: Arc<dyn StorageBackend>) -> Result<()> {
    let app = router(storage);
    axum::serve(listener, app).await.map_err(AppError::Io)?;
    Ok(())
}
