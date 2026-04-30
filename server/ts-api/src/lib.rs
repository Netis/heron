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

pub mod extractors;
pub mod params;
pub mod response;
pub mod routes;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use ts_common::config::{ApiConfig, AppConfig};
use ts_common::error::{AppError, Result};
use ts_common::internal_metrics::MetricsSvc;
use ts_storage::StorageBackend;

/// Carriers for `/api/internal-metrics` — every per-pipeline `MetricsSvc`
/// plus the cross-pipeline (storage) one. Build this in `main.rs` after
/// `MetricsSystem::start()`.
#[derive(Clone)]
pub struct ApiMetricsContext {
    pub pipelines: Vec<(String, Arc<MetricsSvc>)>,
    pub global: Arc<MetricsSvc>,
}

/// Carrier for `/api/runtime-config` — the live in-memory `AppConfig`
/// (with CLI/env overrides already baked in) plus load metadata so the UI
/// can prove "this is what the running process is using right now".
#[derive(Clone)]
pub struct ApiRuntimeConfigContext {
    pub config: Arc<AppConfig>,
    pub config_path: String,
    pub loaded_at_ms: i64,
    pub version: &'static str,
}

/// Carrier for `/api/health` — minimal liveness data (uptime + which
/// pipelines were registered at startup). Built from `loaded_at_ms` and
/// the names of `ApiMetricsContext.pipelines` to avoid taking another
/// reference to those `Arc<MetricsSvc>`s.
#[derive(Clone)]
pub struct ApiHealthContext {
    pub started_at_ms: i64,
    pub version: &'static str,
    pub pipelines: Vec<String>,
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
pub fn router(
    storage: Arc<dyn StorageBackend>,
    metrics: ApiMetricsContext,
    runtime_config: ApiRuntimeConfigContext,
    health: ApiHealthContext,
) -> Router {
    let internal_metrics_routes = Router::new()
        .route(
            "/api/internal-metrics",
            get(routes::internal_metrics::internal_metrics),
        )
        .with_state(metrics);

    let runtime_config_routes = Router::new()
        .route(
            "/api/runtime-config",
            get(routes::runtime_config::runtime_config),
        )
        .with_state(runtime_config);

    let health_routes = Router::new()
        .route("/api/health", get(routes::health::health))
        .with_state(health);

    Router::new()
        .route("/api/filters/wire-apis", get(routes::filters::wire_apis))
        .route("/api/filters/models", get(routes::filters::models))
        .route("/api/filters/server-ips", get(routes::filters::server_ips))
        .route(
            "/api/filters/finish-reasons",
            get(routes::filters::finish_reasons),
        )
        .route("/api/metrics/timeseries", get(routes::metrics::timeseries))
        .route("/api/metrics/summary", get(routes::metrics::summary))
        .route("/api/metrics/models", get(routes::metrics::models))
        .route(
            "/api/metrics/finish-reasons",
            get(routes::metrics::finish_reasons),
        )
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
        .with_state(storage)
        .merge(internal_metrics_routes)
        .merge(runtime_config_routes)
        .merge(health_routes)
        .layer(CorsLayer::permissive())
}
