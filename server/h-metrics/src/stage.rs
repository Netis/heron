//! Metrics aggregation stage: spawns M parallel MetricsAggregator tasks,
//! each shard keyed by hash(wire_api, model, server_ip) % M. Window close
//! is purely event-timestamp driven (no wall-clock tick).
//!
//! Each aggregator handles multiple sources (identified by `source_id` on
//! each event). Per-source watermarks ensure window close is independent
//! across sources.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use h_common::internal_metrics::{Metric, MetricsSystem};
use h_llm::model::LlmEvent;

use crate::active::ActiveCalls;
use crate::aggregator::MetricsAggregator;
use crate::model::LlmMetricsBatch;

/// Spawn one metrics-aggregator task per shard (inferred from `shard_rxs.len()`).
/// Panics on empty `shard_rxs` — a wiring bug in the composition root.
pub fn spawn_metrics_stage(
    shard_rxs: Vec<mpsc::Receiver<LlmEvent>>,
    metrics_tx: mpsc::Sender<LlmMetricsBatch>,
    metrics_sys: &mut MetricsSystem,
) -> Vec<JoinHandle<()>> {
    assert!(
        !shard_rxs.is_empty(),
        "spawn_metrics_stage: shard_rxs must be non-empty"
    );

    // Per-shard open-bucket gauges (sawtooth: in-window slots awaiting
    // drain); the probe sums across shards.
    let bucket_gauges: Vec<Arc<AtomicU64>> = (0..shard_rxs.len())
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    let bucket_probe = bucket_gauges.clone();
    metrics_sys.register_queue_probe(Metric::MetricsAggregatorOpenBuckets, move || {
        bucket_probe.iter().map(|g| g.load(Ordering::Relaxed)).sum()
    });

    // One in-flight table for every shard. Shards are chosen by a hash that
    // includes the model, and a call's Start and Complete can name different
    // models — so they can land on different shards. See `crate::active`.
    let active_calls = ActiveCalls::new();
    let open_probe = active_calls.clone();
    metrics_sys.register_queue_probe(Metric::MetricsActiveCallsOpen, move || {
        open_probe.open_count() as u64
    });

    let mut handles = Vec::with_capacity(shard_rxs.len());
    for (i, (mut rx, bucket_gauge)) in shard_rxs
        .into_iter()
        .zip(bucket_gauges.into_iter())
        .enumerate()
    {
        let active_calls = active_calls.clone();
        let metrics_tx = metrics_tx.clone();
        let worker_metrics = metrics_sys.register_worker(
            &format!("metrics.{i}"),
            &[
                Metric::MetricsLlmEventsStart,
                Metric::MetricsLlmEventsComplete,
                Metric::MetricsHeartbeatsReceived,
                Metric::MetricsWindowsEmitted,
                Metric::MetricsActiveCallsExpired,
            ],
        );
        handles.push(tokio::spawn(async move {
            let shard = i;
            let mut agg = MetricsAggregator::with_active_calls(worker_metrics, active_calls);
            let reason = 'main: loop {
                let event = match rx.recv().await {
                    Some(e) => e,
                    None => break 'main "upstream_eof",
                };
                for m in agg.process(&event) {
                    if metrics_tx.send(m).await.is_err() {
                        break 'main "downstream_closed";
                    }
                }
                bucket_gauge.store(agg.open_bucket_count() as u64, Ordering::Relaxed);
            };
            match reason {
                "upstream_eof" => {
                    // Drain the final window only when upstream closed cleanly.
                    for m in agg.flush_all() {
                        let _ = metrics_tx.send(m).await;
                    }
                    bucket_gauge.store(agg.open_bucket_count() as u64, Ordering::Relaxed);
                    tracing::debug!(shard, "metrics worker stopping: upstream EOF");
                }
                r => {
                    tracing::warn!(
                        shard,
                        reason = r,
                        "metrics worker stopping: downstream closed"
                    );
                }
            }
        }));
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use h_llm::model::{AgentCallInfo, ApiType, LlmCall, LlmCallStart};
    use h_llm::wire_apis as wa;

    fn start_event(ts_us: i64, model: &str) -> LlmEvent {
        LlmEvent::Start(LlmCallStart {
            source_id: String::new(),
            wire_api: wa::OPENAI_CHAT,
            model: model.into(),
            is_stream: true,
            // Same connection as the Complete helpers below, so a Start and a
            // Complete with the same timestamp are one call.
            client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
            client_port: 12345,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            server_port: 443,
            timestamp_us: ts_us,
        })
    }

    fn complete_event(ts_us: i64, model: &str) -> LlmEvent {
        LlmEvent::Complete {
            call: std::sync::Arc::new(LlmCall {
                source_id: String::new(),
                id: format!("c-{ts_us}"),
                wire_api: wa::OPENAI_CHAT,
                model: model.into(),
                api_type: ApiType::Chat,
                request_time: ts_us,
                response_time: Some(ts_us + 100_000),
                complete_time: Some(ts_us + 200_000),
                request_path: "/v1/chat".into(),
                is_stream: true,
                request_body: None,
                status_code: Some(200),
                finish_reason: Some("stop".to_string()),
                response_body: None,
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                ttft_ms: Some(100.0),
                e2e_latency_ms: Some(200.0),
                client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
                client_port: 12345,
                server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
                server_port: 443,
                response_id: None,
                request_headers: vec![],
                response_headers: vec![],
                is_agent_request: false,
                tool_surface: None,
                agent_topology: None,
                tool_call_count: 0,
                tool_names: vec![],
                body_bytes_dropped: 0,
                process: None,
            }),
            agent: Some(AgentCallInfo {
                agent_kind: "x",
                session_id: "s".into(),
                subagent_name: None,
                is_user_turn_start: None,
                is_turn_terminal: false,
                is_auxiliary: false,
                user_input: None,
                assistant_text: None,
                is_agent_request: false,
                tool_surface: None,
                agent_topology: None,
                tool_names: Vec::new(),
                tool_call_count: 0,
                suspicious_signals: Vec::new(),
            }),
        }
    }

    #[tokio::test]
    async fn single_shard_produces_metrics() {
        let (tx, rx) = mpsc::channel::<LlmEvent>(16);
        let (mtx, mut mrx) = mpsc::channel::<LlmMetricsBatch>(64);

        let mut sys = MetricsSystem::new();
        spawn_metrics_stage(vec![rx], mtx.clone(), &mut sys);
        let _svc = sys.start();
        drop(mtx);

        tx.send(start_event(1_000_000_000_000, "gpt-4"))
            .await
            .unwrap();
        tx.send(complete_event(1_000_000_000_000, "gpt-4"))
            .await
            .unwrap();
        drop(tx);

        let mut metrics = Vec::new();
        while let Some(m) = mrx.recv().await {
            metrics.push(m);
        }
        // flush_all emits 4 granularities × 4 dimensions = 16 merged rows
        // for one call.
        assert_eq!(metrics.len(), 16);
    }

    #[tokio::test]
    async fn four_shards_aggregate_independently() {
        let mut txs = Vec::with_capacity(4);
        let mut rxs = Vec::with_capacity(4);
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel::<LlmEvent>(16);
            txs.push(tx);
            rxs.push(rx);
        }
        let (mtx, mut mrx) = mpsc::channel::<LlmMetricsBatch>(256);
        let mut sys = MetricsSystem::new();
        spawn_metrics_stage(rxs, mtx.clone(), &mut sys);
        let _svc = sys.start();
        drop(mtx);

        for (i, tx) in txs.iter().enumerate() {
            let ts = 1_000_000_000_000 + i as i64 * 1_000_000;
            tx.send(start_event(ts, "gpt-4")).await.unwrap();
            tx.send(complete_event(ts, "gpt-4")).await.unwrap();
        }
        drop(txs);

        let mut metrics = Vec::new();
        while let Some(m) = mrx.recv().await {
            metrics.push(m);
        }
        assert_eq!(metrics.len(), 64, "4 shards × 16 metrics each");
    }

    #[tokio::test]
    #[should_panic(expected = "spawn_metrics_stage: shard_rxs must be non-empty")]
    async fn panics_on_empty_shard_rxs() {
        let (_mtx, _mrx) = mpsc::channel::<LlmMetricsBatch>(1);
        let mut sys = MetricsSystem::new();
        spawn_metrics_stage(vec![], _mtx, &mut sys);
    }
}
