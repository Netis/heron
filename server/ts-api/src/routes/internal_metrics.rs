//! `GET /api/internal-metrics` — current snapshot of every registered
//! internal metric across all pipelines plus the global (storage) view.
//!
//! `GET /api/internal-metrics/series` — short-window time-series for the
//! handful of gauges the `AggregateHistory` ring is tracking (currently
//! `flows_active` and `turns_active`). Used by the dashboard's
//! "Active TCP Connections" / "Active Agent Sessions" charts.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use ts_common::internal_metrics::{Metric, MetricKind, MetricsSvc};

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

// ---------------------------------------------------------------------------
// /api/internal-metrics/series
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SeriesQuery {
    /// Optional unix-ms cutoff: only return points with `ts_ms >= since`.
    /// Defaults to 0 (return everything in the ring).
    #[serde(default)]
    pub since: Option<i64>,
    /// Comma-separated short metric names (e.g. `"flows_active,turns_active"`).
    /// Defaults to whatever the ring is tracking — typically all of them.
    #[serde(default)]
    pub metrics: Option<String>,
}

#[derive(Serialize)]
struct SeriesPoint {
    /// Unix epoch milliseconds.
    t: i64,
    /// Gauge value at that instant (summed across all contributor svcs).
    v: u64,
}

#[derive(Serialize)]
struct SeriesResponse {
    ts: i64,
    /// One entry per requested metric. Empty array if the history ring is
    /// not initialized (i.e. internal_metrics disabled).
    series: Vec<SeriesEntry>,
}

#[derive(Serialize)]
struct SeriesEntry {
    name: &'static str,
    group: &'static str,
    points: Vec<SeriesPoint>,
}

fn resolve_metric(short: &str) -> Option<Metric> {
    Metric::ALL
        .iter()
        .copied()
        .find(|m| m.spec().short_name == short)
}

pub async fn series(
    State(ctx): State<ApiMetricsContext>,
    Query(q): Query<SeriesQuery>,
) -> impl IntoResponse {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let Some(history) = ctx.history.as_ref() else {
        return ApiResponse::ok(SeriesResponse {
            ts,
            series: Vec::new(),
        });
    };

    let since = q.since.unwrap_or(0);
    let requested: Vec<Metric> = match q.metrics.as_deref() {
        Some(csv) => csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(resolve_metric)
            .filter(|m| history.tracked().contains(m))
            .collect(),
        None => history.tracked().to_vec(),
    };

    // De-dup while preserving order so `metrics=a,a,b` doesn't return two
    // identical series entries.
    let mut seen: BTreeMap<Metric, ()> = BTreeMap::new();
    let series = requested
        .into_iter()
        .filter(|m| seen.insert(*m, ()).is_none())
        .map(|m| {
            let pts = history
                .series(m, since)
                .into_iter()
                .map(|p| SeriesPoint {
                    t: p.ts_ms,
                    v: p.value,
                })
                .collect();
            let spec = m.spec();
            SeriesEntry {
                name: spec.short_name,
                group: spec.group.as_str(),
                points: pts,
            }
        })
        .collect();

    ApiResponse::ok(SeriesResponse { ts, series })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use ts_common::internal_metrics::{AggregateHistory, Metric, MetricsSystem};

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
            history: None,
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
            history: None,
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

    fn build_history(tracked: Vec<Metric>) -> Arc<AggregateHistory> {
        let history = AggregateHistory::new(tracked.clone(), 16);
        for (ts, vals) in [
            (
                1_000i64,
                &[(Metric::FlowsActive, 3u64), (Metric::TurnActive, 1)][..],
            ),
            (2_000, &[(Metric::FlowsActive, 5), (Metric::TurnActive, 2)]),
            (3_000, &[(Metric::FlowsActive, 4), (Metric::TurnActive, 2)]),
        ] {
            let mut frame: BTreeMap<Metric, u64> = BTreeMap::new();
            for (m, v) in vals {
                if tracked.contains(m) {
                    frame.insert(*m, *v);
                }
            }
            history.push(ts, frame);
        }
        history
    }

    fn ctx_with_history(history: Arc<AggregateHistory>) -> ApiMetricsContext {
        ApiMetricsContext {
            pipelines: vec![],
            global: build_global_svc(),
            history: Some(history),
        }
    }

    #[tokio::test]
    async fn series_returns_tracked_metrics_by_default() {
        let history = build_history(vec![Metric::FlowsActive, Metric::TurnActive]);
        let resp = series(
            State(ctx_with_history(history)),
            Query(SeriesQuery {
                since: None,
                metrics: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let series = v["data"]["series"].as_array().unwrap();
        assert_eq!(series.len(), 2);

        let flows = series.iter().find(|s| s["name"] == "flows_active").unwrap();
        let pts = flows["points"].as_array().unwrap();
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0]["t"], 1000);
        assert_eq!(pts[0]["v"], 3);
        assert_eq!(pts[2]["v"], 4);
    }

    #[tokio::test]
    async fn series_honors_since_cutoff() {
        let history = build_history(vec![Metric::FlowsActive, Metric::TurnActive]);
        let resp = series(
            State(ctx_with_history(history)),
            Query(SeriesQuery {
                since: Some(2_500),
                metrics: Some("flows_active".to_string()),
            }),
        )
        .await
        .into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let series = v["data"]["series"].as_array().unwrap();
        assert_eq!(series.len(), 1);
        let pts = series[0]["points"].as_array().unwrap();
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0]["t"], 3000);
    }

    #[tokio::test]
    async fn series_filters_to_tracked_only_and_dedups() {
        let history = build_history(vec![Metric::FlowsActive]);
        let resp = series(
            State(ctx_with_history(history)),
            Query(SeriesQuery {
                since: None,
                metrics: Some("flows_active,turns_active,flows_active".to_string()),
            }),
        )
        .await
        .into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let series = v["data"]["series"].as_array().unwrap();
        assert_eq!(series.len(), 1, "untracked + dup should both be dropped");
        assert_eq!(series[0]["name"], "flows_active");
    }

    #[tokio::test]
    async fn series_without_history_yields_empty_array() {
        let ctx = ApiMetricsContext {
            pipelines: vec![],
            global: build_global_svc(),
            history: None,
        };
        let resp = series(
            State(ctx),
            Query(SeriesQuery {
                since: None,
                metrics: None,
            }),
        )
        .await
        .into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 0);
        assert_eq!(v["data"]["series"].as_array().unwrap().len(), 0);
    }
}
