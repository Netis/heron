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
        .route("/api/filters/wire-apis", get(routes::filters::wire_apis))
        .route("/api/filters/models", get(routes::filters::models))
        .route("/api/filters/server-ips", get(routes::filters::server_ips))
        .route("/api/metrics/timeseries", get(routes::metrics::timeseries))
        .route("/api/metrics/summary", get(routes::metrics::summary))
        .route("/api/metrics/models", get(routes::metrics::models))
        .route("/api/llm-calls", get(routes::llm_calls::list))
        .route("/api/llm-calls/{id}", get(routes::llm_calls::detail))
        .route("/api/http-exchanges", get(routes::http_exchanges::list))
        .route(
            "/api/http-exchanges/{id}",
            get(routes::http_exchanges::detail),
        )
        .route("/api/agent-turns", get(routes::agent_turns::list))
        .route("/api/agent-turns/{id}", get(routes::agent_turns::detail))
        .route(
            "/api/agent-turns/{id}/calls",
            get(routes::agent_turns::calls),
        )
        .route("/api/agent-sessions", get(routes::agent_sessions::list))
        .route(
            "/api/agent-sessions/{source_id}/{session_id}",
            get(routes::agent_sessions::detail),
        )
        .route(
            "/api/agent-sessions/{source_id}/{session_id}/turns",
            get(routes::agent_sessions::turns),
        )
        .layer(CorsLayer::permissive())
        .with_state(storage)
}

/// Serve the API on an already-bound listener (runs until shutdown).
pub async fn serve(listener: TcpListener, storage: Arc<dyn StorageBackend>) -> Result<()> {
    let app = router(storage);
    axum::serve(listener, app).await.map_err(AppError::Io)?;
    Ok(())
}
