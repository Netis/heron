//! `GET /api/runtime-config` — the in-memory `AppConfig` actually driving
//! the running process (post env-var and CLI-flag overrides), plus minimal
//! load metadata. Intentionally *not* a re-read of the on-disk TOML.

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use ts_common::config::AppConfig;

use crate::response::ApiResponse;
use crate::ApiRuntimeConfigContext;

#[derive(Serialize)]
struct RuntimeConfigResponse {
    loaded_at_ms: i64,
    config_path: String,
    version: &'static str,
    config: AppConfig,
}

pub async fn runtime_config(State(ctx): State<ApiRuntimeConfigContext>) -> impl IntoResponse {
    ApiResponse::ok(RuntimeConfigResponse {
        loaded_at_ms: ctx.loaded_at_ms,
        config_path: ctx.config_path,
        version: ctx.version,
        config: (*ctx.config).clone(),
    })
}
