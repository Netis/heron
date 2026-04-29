//! `GET /api/internal-metrics` — current snapshot of every registered
//! internal metric across all pipelines plus the global (storage) view.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use ts_common::internal_metrics::{MetricKind, MetricsSvc};

use crate::response::ApiResponse;
use crate::ApiMetricsContext;

#[derive(Serialize)]
struct InternalMetricsResponse {
    ts: i64,
    pipelines: Vec<PipelineSnapshot>,
    global: SnapshotPayload,
}

#[derive(Serialize)]
struct PipelineSnapshot {
    name: String,
    #[serde(flatten)]
    snapshot: SnapshotPayload,
}

#[derive(Serialize)]
struct SnapshotPayload {
    metrics: Vec<MetricRecord>,
}

#[derive(Serialize)]
struct MetricRecord {
    name: &'static str,
    group: &'static str,
    kind: &'static str,
    value: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    capacity: Option<u64>,
}

fn render_snapshot(svc: &MetricsSvc) -> SnapshotPayload {
    svc.sample_probes();
    let snap = svc.snapshot();
    let caps = svc.capacities();
    let metrics = snap
        .values
        .iter()
        .map(|(metric, &value)| {
            let spec = metric.spec();
            let kind = match spec.kind {
                MetricKind::Counter => "counter",
                MetricKind::Gauge => "gauge",
            };
            MetricRecord {
                name: spec.short_name,
                group: spec.group.as_str(),
                kind,
                value,
                capacity: caps.get(metric).copied(),
            }
        })
        .collect::<Vec<_>>();
    SnapshotPayload { metrics }
}

pub async fn internal_metrics(State(ctx): State<ApiMetricsContext>) -> impl IntoResponse {
    let pipelines = ctx
        .pipelines
        .iter()
        .map(|(name, svc)| PipelineSnapshot {
            name: name.clone(),
            snapshot: render_snapshot(svc),
        })
        .collect::<Vec<_>>();
    let global = render_snapshot(&ctx.global);

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    ApiResponse::ok(InternalMetricsResponse {
        ts,
        pipelines,
        global,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use std::sync::Arc;
    use ts_common::internal_metrics::{Metric, MetricsSystem};

    fn build_pipeline_svc() -> Arc<MetricsSvc> {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[Metric::CapturePacketsReceived, Metric::NetPacketsParsed],
        );
        sys.register_queue_probe_capped(Metric::QueueDepthRaw, 4096, || 4000);
        let svc = sys.start();
        w.counter(Metric::CapturePacketsReceived).add(123);
        w.counter(Metric::NetPacketsParsed).add(120);
        svc
    }

    fn build_global_svc() -> Arc<MetricsSvc> {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker("storage", &[Metric::StorageFlushedCalls]);
        sys.register_queue_probe_capped(Metric::StorageQueueDepthCalls, 1024, || 7);
        let svc = sys.start();
        w.counter(Metric::StorageFlushedCalls).add(50);
        svc
    }

    #[tokio::test]
    async fn snapshot_includes_pipeline_and_global() {
        let ctx = ApiMetricsContext {
            pipelines: vec![("default".to_string(), build_pipeline_svc())],
            global: build_global_svc(),
        };
        let resp = internal_metrics(State(ctx)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 0);
        assert!(v["data"]["ts"].as_i64().unwrap() > 0);

        let pipelines = v["data"]["pipelines"].as_array().unwrap();
        assert_eq!(pipelines.len(), 1);
        assert_eq!(pipelines[0]["name"], "default");

        let metrics = pipelines[0]["metrics"].as_array().unwrap();
        let pkts = metrics
            .iter()
            .find(|m| m["name"] == "pkts_received")
            .expect("pkts_received in pipeline snapshot");
        assert_eq!(pkts["value"], 123);
        assert_eq!(pkts["kind"], "counter");
        assert_eq!(pkts["group"], "capture");
        assert!(pkts.get("capacity").map(|c| c.is_null()).unwrap_or(true));

        let q_raw = metrics
            .iter()
            .find(|m| m["name"] == "q_raw_pkts")
            .expect("q_raw_pkts in pipeline snapshot");
        assert_eq!(q_raw["value"], 4000);
        assert_eq!(q_raw["capacity"], 4096);
        assert_eq!(q_raw["kind"], "gauge");

        let global_metrics = v["data"]["global"]["metrics"].as_array().unwrap();
        let flushed = global_metrics
            .iter()
            .find(|m| m["name"] == "flushed_calls")
            .expect("flushed_calls in global snapshot");
        assert_eq!(flushed["value"], 50);
    }

    #[tokio::test]
    async fn empty_pipelines_yields_empty_array() {
        let ctx = ApiMetricsContext {
            pipelines: vec![],
            global: build_global_svc(),
        };
        let resp = internal_metrics(State(ctx)).await.into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"]["pipelines"].as_array().unwrap().len(), 0);
        assert!(!v["data"]["global"]["metrics"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
