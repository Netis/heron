//! `GET /api/health` — liveness probe for the running tokenscope process.
//!
//! Always returns 200 with `code: 0` when the route is reachable. The body
//! reports per-pipeline `running`, derived from a single `drained` flag set
//! by `main.rs`: it stays `true` while capture sources are feeding the
//! pipeline, and flips to `false` once every source has hit EOF and the
//! stage tasks have drained (the process then parks on a shutdown signal so
//! the API/console remain available for inspection — pass
//! `--exit-after-drain` to opt back into the old "exit when done" behavior).
//! This is *not* a process supervisor; a panicked stage will not be reflected
//! in `running` — use `/api/internal-metrics` for queue/drop diagnostics.

use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::response::ApiResponse;
use crate::ApiHealthContext;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    uptime_secs: i64,
    pipelines: Vec<PipelineHealth>,
}

#[derive(Serialize)]
struct PipelineHealth {
    name: String,
    running: bool,
}

pub async fn health(State(ctx): State<ApiHealthContext>) -> impl IntoResponse {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let uptime_secs = ((now_ms - ctx.started_at_ms).max(0)) / 1000;

    let running = !ctx.drained.load(Ordering::Acquire);
    let pipelines = ctx
        .pipelines
        .iter()
        .map(|name| PipelineHealth {
            name: name.clone(),
            running,
        })
        .collect();

    ApiResponse::ok(HealthResponse {
        status: "ready",
        version: ctx.version,
        uptime_secs,
        pipelines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[tokio::test]
    async fn health_reports_pipelines_and_uptime() {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            - 5_000;
        let ctx = ApiHealthContext {
            started_at_ms: started,
            version: "0.1.0-test",
            pipelines: vec!["local".to_string(), "remote".to_string()],
            drained: Arc::new(AtomicBool::new(false)),
        };
        let resp = health(State(ctx)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 0);
        assert_eq!(v["data"]["status"], "ready");
        assert_eq!(v["data"]["version"], "0.1.0-test");
        let uptime = v["data"]["uptime_secs"].as_i64().unwrap();
        assert!((4..=10).contains(&uptime), "uptime {uptime} not in [4,10]");
        let pipelines = v["data"]["pipelines"].as_array().unwrap();
        assert_eq!(pipelines.len(), 2);
        assert_eq!(pipelines[0]["name"], "local");
        assert_eq!(pipelines[0]["running"], true);
    }

    #[tokio::test]
    async fn health_with_no_pipelines() {
        let ctx = ApiHealthContext {
            started_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
            version: "0.1.0-test",
            pipelines: Vec::new(),
            drained: Arc::new(AtomicBool::new(false)),
        };
        let resp = health(State(ctx)).await.into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"]["pipelines"].as_array().unwrap().len(), 0);
        assert_eq!(v["data"]["status"], "ready");
    }

    #[tokio::test]
    async fn health_reports_drained_pipelines() {
        let drained = Arc::new(AtomicBool::new(true));
        let ctx = ApiHealthContext {
            started_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
            version: "0.1.0-test",
            pipelines: vec!["cli".to_string(), "remote".to_string()],
            drained,
        };
        let resp = health(State(ctx)).await.into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let pipelines = v["data"]["pipelines"].as_array().unwrap();
        assert_eq!(pipelines.len(), 2);
        for p in pipelines {
            assert_eq!(p["running"], false, "pipeline {p:?} should be drained");
        }
        assert_eq!(v["data"]["status"], "ready");
    }
}
