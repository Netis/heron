//! Composition root for the ingest pipeline.
//!
//! Builds **one pipeline per [`PipelineDef`]** — D dispatchers → protocol →
//! llm → turn → **metrics** — with N sources fan-in through a
//! [`RoutingSender`] that routes by `hash(stream_id) % D` to D dispatcher
//! channels (default D=1). Flow keys, HTTP reassembly state,
//! `LlmCall`/`LlmTurn` state, and the metrics aggregator's event-time
//! watermark never leak between pipelines. Only the storage sink is shared
//! across pipelines so every record lands in the same DB tables.
//!
//! Per-pipeline metrics matter because pipelines (local pcap + cloud-probe,
//! different hosts, etc.) have skewed clocks. Each aggregator maintains
//! per-stream watermarks keyed by the `stream_id` carried on every event.
//! For pcap sources one pipeline == one stream; for cloud-probe sources a
//! single pipeline may carry many streams (one per remote probe UUID).
//! The storage sink / query layer can merge per-stream rows explicitly.
//!
//! Exposes:
//!
//! * `pipeline_txs` — one `(name, RoutingSender)` per pipeline, matching
//!   the order of `pipeline_defs`. Each capture source pushes into the
//!   routing sender of the pipeline that owns it.
//! * `pipeline_sources` — the `CaptureSourceConfig` lists per pipeline, so
//!   the caller knows which sources to spawn for each pipeline sender.
//! * `stage_handles` — every detached stage task (including every pipeline's
//!   metrics stage and the shared storage sink), for panic supervision and
//!   final-drain awaiting. Each handle is labeled with a `StageTask` that
//!   names the pipeline it belongs to (if any) plus the stage and shard.
//!
//! Shutdown semantics: dropping every entry in `pipeline_txs` (plus letting
//! capture sources stop) cascades EOF down each pipeline. Per-pipeline
//! stages close their shared-sender clones on exit; the per-pipeline metrics
//! stage drains once its llm stage finishes, and once every pipeline's llm
//! stage has drained the shared `calls_tx`/`turns_tx` and every metrics
//! stage clone of `metrics_out_tx` are dropped — the shared sink observes
//! EOF and flushes. Panics are surfaced via [`Pipeline::supervise`], which
//! resolves as soon as any stage task exits with an error (panic / cancel).
//! Once every stage — including the sink — exits cleanly, `supervise`
//! returns `None` and the pipeline is fully drained.

use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc::{self, WeakSender};
use tokio::task::{JoinHandle, JoinSet};

use ts_capture::{RawPacket, RoutingSender};
use ts_common::config::{CaptureSourceConfig, PipelineDef};
use ts_common::internal_metrics::{Metric, MetricsSystem};
use ts_llm::model::{LlmCall, LlmEvent, TurnShardInput};
use ts_metrics::model::LlmMetric;
use ts_protocol::{spawn_flow_dispatcher, spawn_protocol_stage, WorkerInput};
use ts_storage::StorageBackend;
use ts_turn::tracker::TrackerConfig;
use ts_turn::LlmTurn;

/// Every task spawned by the pipeline is labeled so panic logs name the
/// specific worker that died. Cheap strings — formatting happens only at
/// log time via `Display`.
///
/// * `pipeline` is `Some(name)` for a per-pipeline stage (dispatcher,
///   protocol, llm, turn, metrics), `None` for shared stages (storage sink).
/// * `shard` is `Some(j)` for a per-shard worker, `None` for singleton
///   stages within their scope (dispatcher per pipeline, storage sink).
#[derive(Debug, Clone)]
pub struct StageTask {
    pub stage: &'static str,
    pub shard: Option<usize>,
    pub pipeline: Option<String>,
}

impl fmt::Display for StageTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref p) = self.pipeline {
            write!(f, "{p}.")?;
        }
        match self.shard {
            Some(i) => write!(f, "{}.{i}", self.stage),
            None => f.write_str(self.stage),
        }
    }
}

pub struct Pipeline {
    /// One `(name, RoutingSender)` per pipeline, in the same order as the
    /// `pipeline_defs` slice passed to [`Pipeline::build`]. Each capture
    /// source must push into the sender of the pipeline that owns it.
    /// The `RoutingSender` transparently routes packets to one of D
    /// dispatcher channels by `hash(stream_id) % D`. When `dispatcher_count = 1`
    /// (the default) the routing is a no-op.
    pub pipeline_txs: Vec<(String, RoutingSender)>,
    /// The capture sources for each pipeline, matching `pipeline_txs` order.
    pub pipeline_sources: Vec<Vec<CaptureSourceConfig>>,
    pub stage_handles: Vec<(StageTask, JoinHandle<()>)>,
}

impl Pipeline {
    /// Build the entire pipeline. Spawns every stage task and returns
    /// handles without blocking.
    ///
    /// `per_pipeline_metrics` contains one [`MetricsSystem`] per pipeline
    /// definition; its length must equal `pipeline_defs.len()`.
    /// Each `MetricsSystem` has `{name}.*` workers registered against it
    /// by the dispatcher/protocol/llm/turn stages; the caller is expected
    /// to start one
    /// [`MetricsReporter`](ts_common::internal_metrics::MetricsReporter) per
    /// system so log lines are per-pipeline.
    ///
    /// The metrics stage is **per-pipeline** (one aggregator group per
    /// `PipelineDef`, registered against that pipeline's `MetricsSystem`).
    /// There is no cross-pipeline metrics stage — the sink is the only truly
    /// shared stage.
    pub fn build(
        pipeline_defs: &[PipelineDef],
        storage_config: &ts_storage::StorageSinkConfig,
        storage: Arc<dyn StorageBackend>,
        per_pipeline_metrics: &mut [MetricsSystem],
    ) -> Self {
        // ---- Shared sinks (fan-in across every pipeline) ----
        // Use the max queue capacity across all pipelines for shared channels.
        let sink_capacity = pipeline_defs
            .iter()
            .map(|d| {
                d.queues
                    .call_sink
                    .max(d.queues.turn_sink)
                    .max(d.queues.metric_sink)
            })
            .max()
            .unwrap_or(4096);

        let (calls_tx, calls_rx) = mpsc::channel::<Arc<LlmCall>>(sink_capacity);
        let (turns_tx, turns_rx) = mpsc::channel::<LlmTurn>(sink_capacity);
        let (metrics_out_tx, metrics_out_rx) = mpsc::channel::<LlmMetric>(sink_capacity);

        let registry = Arc::new(ts_llm::profiles::build_default_registry());

        assert_eq!(
            per_pipeline_metrics.len(),
            pipeline_defs.len(),
            "per_pipeline_metrics length must match pipeline_defs length"
        );

        // Shared sink queue depth probes — registered against the first
        // pipeline's MetricsSystem (same as the storage_sink worker).
        {
            let w = calls_tx.downgrade();
            per_pipeline_metrics[0].register_queue_probe(Metric::QueueDepthCalls, move || {
                w.upgrade()
                    .map_or(0, |s| (s.max_capacity() - s.capacity()) as u64)
            });
            let w = turns_tx.downgrade();
            per_pipeline_metrics[0].register_queue_probe(Metric::QueueDepthTurns, move || {
                w.upgrade()
                    .map_or(0, |s| (s.max_capacity() - s.capacity()) as u64)
            });
            let w = metrics_out_tx.downgrade();
            per_pipeline_metrics[0].register_queue_probe(Metric::QueueDepthMetricsOut, move || {
                w.upgrade()
                    .map_or(0, |s| (s.max_capacity() - s.capacity()) as u64)
            });
        }

        let mut stage_handles: Vec<(StageTask, JoinHandle<()>)> = Vec::new();
        let mut pipeline_txs: Vec<(String, RoutingSender)> =
            Vec::with_capacity(pipeline_defs.len());
        let mut pipeline_sources: Vec<Vec<CaptureSourceConfig>> =
            Vec::with_capacity(pipeline_defs.len());

        // ---- Per-pipeline sub-pipelines ----
        // Each iteration builds an isolated dispatcher → protocol → llm →
        // turn → metrics chain that fans **out** into shared sinks
        // (`calls_tx`/`turns_tx`/`metrics_out_tx`). The shared senders are
        // cloned here, not moved; the outer clones are dropped after the
        // loop so the sink observes EOF once every pipeline finishes.
        // The metrics stage is inside this loop so each pipeline gets its
        // own aggregator with per-stream watermarks, avoiding cross-source
        // clock-skew re-emit.
        for (def, metrics_sys) in pipeline_defs.iter().zip(per_pipeline_metrics.iter_mut()) {
            let name = &def.name;
            let dispatcher_count = def.dispatcher_count;
            let flow_shards = def.flow_shard_count;
            let turn_shards = def.turn.shard_count;
            let metrics_shards = def.metrics.shard_count;
            let q = &def.queues;

            let tracker_cfg = TrackerConfig {
                idle_timeout_us: (def.turn.idle_timeout_secs as i64) * 1_000_000,
                sweep_interval_us: (def.turn.sweep_interval_secs as i64) * 1_000_000,
                grace: std::time::Duration::from_millis(def.turn.grace_ms),
            };

            let (parsed_txs, parsed_rxs) =
                make_shard_channels::<WorkerInput>(flow_shards, q.parsed_packet);
            // Capture weaks now; `parsed_txs` originals are dropped after
            // cloning into dispatchers, but the weaks remain upgradeable as
            // long as any dispatcher task holds a strong clone of the same channel.
            let parsed_weaks: Vec<WeakSender<WorkerInput>> =
                parsed_txs.iter().map(|tx| tx.downgrade()).collect();
            let (event_txs, event_rxs) =
                make_shard_channels::<ts_protocol::model::ProtocolEvent>(flow_shards, q.flow_event);
            let event_weaks: Vec<WeakSender<ts_protocol::model::ProtocolEvent>> =
                event_txs.iter().map(|tx| tx.downgrade()).collect();
            let (turn_shard_txs, turn_shard_rxs) =
                make_shard_channels::<TurnShardInput>(turn_shards, q.turn_event);
            let turn_shard_weaks: Vec<WeakSender<TurnShardInput>> =
                turn_shard_txs.iter().map(|tx| tx.downgrade()).collect();
            let (metrics_shard_txs, metrics_shard_rxs) =
                make_shard_channels::<LlmEvent>(metrics_shards, q.metrics_event);
            let metrics_shard_weaks: Vec<WeakSender<LlmEvent>> =
                metrics_shard_txs.iter().map(|tx| tx.downgrade()).collect();

            // Dispatchers — D tasks per pipeline, each with its own raw
            // channel. All dispatchers share the same `parsed_txs` (cloned)
            // so packets from any dispatcher reach the correct flow shard.
            // The `RoutingSender` routes packets to dispatchers by
            // `hash(stream_id) % D`.
            let mut disp_txs = Vec::with_capacity(dispatcher_count);
            for i in 0..dispatcher_count {
                let (dtx, drx) = mpsc::channel::<RawPacket>(q.raw);
                disp_txs.push(dtx);
                let worker_txs_clone: Vec<_> = parsed_txs.iter().map(|tx| tx.clone()).collect();
                let worker_name = if dispatcher_count > 1 {
                    format!("dispatcher.{i}")
                } else {
                    "dispatcher".to_string()
                };
                let handle =
                    spawn_flow_dispatcher(drx, worker_txs_clone, &worker_name, metrics_sys);
                stage_handles.push((
                    StageTask {
                        stage: "dispatcher",
                        shard: if dispatcher_count > 1 { Some(i) } else { None },
                        pipeline: Some(name.clone()),
                    },
                    handle,
                ));
            }
            // Drop the original parsed_txs senders — only the clones held by
            // dispatchers should keep the worker channels alive.
            drop(parsed_txs);

            // Protocol parser stage — `flow_shards` workers per pipeline.
            let protocol_handles = spawn_protocol_stage(parsed_rxs, event_txs, metrics_sys);
            debug_assert_eq!(protocol_handles.len(), flow_shards);
            for (j, h) in protocol_handles.into_iter().enumerate() {
                stage_handles.push((
                    StageTask {
                        stage: "protocol",
                        shard: Some(j),
                        pipeline: Some(name.clone()),
                    },
                    h,
                ));
            }

            // LLM stage — `flow_shards` workers per pipeline. `metrics_shard_txs`
            // is moved in (pipeline-local, never leaves the loop). `calls_tx` is
            // cloned so the shared sink sees EOF only after every pipeline's
            // llm stage drains.
            let llm_handles = ts_llm::spawn_llm_stage(
                event_rxs,
                turn_shard_txs,
                metrics_shard_txs,
                calls_tx.clone(),
                registry.clone(),
                metrics_sys,
            );
            debug_assert_eq!(llm_handles.len(), flow_shards);
            for (j, h) in llm_handles.into_iter().enumerate() {
                stage_handles.push((
                    StageTask {
                        stage: "llm",
                        shard: Some(j),
                        pipeline: Some(name.clone()),
                    },
                    h,
                ));
            }

            // Turn stage — `turn_shards` workers per pipeline. Feeds the
            // shared `turns_tx`.
            let turn_handles = ts_turn::spawn_turn_stage(
                tracker_cfg,
                turn_shard_rxs,
                turns_tx.clone(),
                registry.clone(),
                metrics_sys,
            );
            debug_assert_eq!(turn_handles.len(), turn_shards);
            for (j, h) in turn_handles.into_iter().enumerate() {
                stage_handles.push((
                    StageTask {
                        stage: "turn",
                        shard: Some(j),
                        pipeline: Some(name.clone()),
                    },
                    h,
                ));
            }

            // Metrics stage — per-pipeline. Owns `metrics_shard_rxs` directly;
            // clones `metrics_out_tx` so each pipeline's metrics tasks drop
            // their sender on drain. The shared sink observes EOF once every
            // pipeline has drained.
            let metrics_handles = ts_metrics::spawn_metrics_stage(
                metrics_shard_rxs,
                metrics_out_tx.clone(),
                metrics_sys,
            );
            debug_assert_eq!(metrics_handles.len(), metrics_shards);
            for (j, h) in metrics_handles.into_iter().enumerate() {
                stage_handles.push((
                    StageTask {
                        stage: "metrics",
                        shard: Some(j),
                        pipeline: Some(name.clone()),
                    },
                    h,
                ));
            }

            // ---- Per-pipeline queue depth probes ----
            let raw_weaks: Vec<WeakSender<RawPacket>> =
                disp_txs.iter().map(|tx| tx.downgrade()).collect();
            metrics_sys.register_queue_probe(Metric::QueueDepthRaw, move || {
                raw_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
            metrics_sys.register_queue_probe(Metric::QueueDepthParsed, move || {
                parsed_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
            metrics_sys.register_queue_probe(Metric::QueueDepthEvent, move || {
                event_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
            metrics_sys.register_queue_probe(Metric::QueueDepthTurnShard, move || {
                turn_shard_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
            metrics_sys.register_queue_probe(Metric::QueueDepthMetricsShard, move || {
                metrics_shard_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });

            pipeline_txs.push((name.clone(), RoutingSender::new(disp_txs)));
            pipeline_sources.push(def.sources.clone());
        }

        // Drop outer clones of shared sink senders: the per-pipeline loop
        // handed a clone to every llm/turn/metrics worker, so these originals
        // are the only remaining references held here. Dropping them lets
        // the sink observe EOF as soon as every pipeline exits.
        drop(calls_tx);
        drop(turns_tx);
        drop(metrics_out_tx);

        // ---- Shared storage sink ----
        // Register against the first pipeline's MetricsSystem so counters
        // appear in that pipeline's reporter output.
        let storage_metrics = per_pipeline_metrics[0].register_worker(
            "storage_sink",
            &[
                Metric::StorageRecordsBuffered,
                Metric::StorageRecordsFlushed,
                Metric::StorageFlushErrors,
            ],
        );
        let sink_handle = ts_storage::spawn_storage_sink_stage(
            storage_config.clone(),
            calls_rx,
            turns_rx,
            metrics_out_rx,
            storage,
            storage_metrics,
        );
        stage_handles.push((
            StageTask {
                stage: "storage_sink",
                shard: None,
                pipeline: None,
            },
            sink_handle,
        ));

        Pipeline {
            pipeline_txs,
            pipeline_sources,
            stage_handles,
        }
    }

    /// Resolves as soon as any stage task exits with a JoinError (panic or
    /// cancel). Clean exits are ignored — a stage returning `Ok(())` is
    /// normal EOF propagation, not a failure.
    ///
    /// If every stage exits cleanly, returns `None` (pipeline drained).
    /// Intended to be `select!`ed alongside shutdown triggers (ctrl-c,
    /// capture completion) so a panicked stage triggers global cancel.
    pub async fn supervise(
        handles: Vec<(StageTask, JoinHandle<()>)>,
    ) -> Option<(StageTask, tokio::task::JoinError)> {
        let mut set: JoinSet<(StageTask, Result<(), tokio::task::JoinError>)> = JoinSet::new();
        for (label, h) in handles {
            set.spawn(async move {
                let res = h.await;
                (label, res)
            });
        }
        while let Some(joined) = set.join_next().await {
            // The outer join never panics (its body just awaits another
            // handle); unwrap is safe.
            let (label, res) = joined.expect("supervise wrapper task joined cleanly");
            if let Err(e) = res {
                return Some((label, e));
            }
        }
        None
    }
}

fn make_shard_channels<T>(
    count: usize,
    capacity: usize,
) -> (Vec<mpsc::Sender<T>>, Vec<mpsc::Receiver<T>>) {
    let mut txs = Vec::with_capacity(count);
    let mut rxs = Vec::with_capacity(count);
    for _ in 0..count {
        let (tx, rx) = mpsc::channel::<T>(capacity);
        txs.push(tx);
        rxs.push(rx);
    }
    (txs, rxs)
}
