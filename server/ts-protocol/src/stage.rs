//! Protocol parsing stage. Split into two spawn helpers so the composition
//! root owns every cross-stage channel (including the dispatcher → parser
//! fan-out):
//!
//! * [`spawn_flow_dispatcher`] — one task that consumes `RawPacket`s
//!   (including heartbeat sentinels), decodes L2-L4 for real packets, and
//!   routes each `ParsedPacket` to `worker_txs[hash(flow_key) % N]`.
//!   Heartbeats (all-zero MACs + `ether_type=0xFFFF`) are broadcast to
//!   every worker so each shard can advance its own clock.
//! * [`spawn_protocol_stage`] — N `FlowWorker` tasks, one per shard,
//!   consuming `WorkerInput`s and producing `ProtocolEvent`s.

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use ts_capture::RawPacket;
use ts_common::internal_metrics::{Metric, MetricsSystem};

use crate::flow::{FlowDispatcher, WorkerInput};
use crate::model::ProtocolEvent;
use crate::tcp::FlowWorker;

/// Spawn the flow dispatcher: one task that consumes `RawPacket`s from
/// `capture_rx` and routes each to `worker_txs`. Real packets are sharded
/// by flow hash; heartbeat sentinels are broadcast to every shard.
///
/// When a pipeline has multiple dispatchers, the composition root creates
/// one channel per dispatcher and wraps the senders in a `RoutingSender`
/// that routes by `hash(stream_id)`. Each dispatcher receives its own
/// clone of `worker_txs` (all pointing to the same set of protocol
/// workers).
pub fn spawn_flow_dispatcher(
    mut capture_rx: mpsc::Receiver<RawPacket>,
    worker_txs: Vec<mpsc::Sender<WorkerInput>>,
    worker_name: &str,
    metrics_sys: &mut MetricsSystem,
) -> JoinHandle<()> {
    assert!(
        !worker_txs.is_empty(),
        "spawn_flow_dispatcher: worker_txs must be non-empty"
    );
    let metrics = metrics_sys.register_worker(
        worker_name,
        &[
            Metric::DispatcherPacketsRouted,
            Metric::DispatcherHeartbeatsDropped,
        ],
    );
    let worker_name = worker_name.to_string();
    tokio::spawn(async move {
        let dispatcher = FlowDispatcher::new(worker_txs, metrics);
        let reason = 'main: loop {
            let raw = match capture_rx.recv().await {
                Some(r) => r,
                None => break 'main "upstream_eof",
            };
            if !dispatcher.dispatch(raw).await {
                break 'main "downstream_closed";
            }
        };
        match reason {
            "upstream_eof" => {
                tracing::debug!(worker = %worker_name, "flow dispatcher stopping: upstream EOF");
            }
            r => {
                tracing::warn!(
                    worker = %worker_name,
                    reason = r,
                    "flow dispatcher stopping: downstream closed"
                );
            }
        }
    })
}

/// Spawn N flow-parser workers, one per input receiver. Each worker consumes
/// `WorkerInput`s from `worker_rxs[i]`, runs TCP reassembly + HTTP/SSE
/// parsing, and emits `ProtocolEvent`s into `event_txs[i]`.
///
/// Panics if `worker_rxs.len() != event_txs.len()` — that is a wiring bug
/// in the composition root, not a runtime condition.
pub fn spawn_protocol_stage(
    worker_rxs: Vec<mpsc::Receiver<WorkerInput>>,
    event_txs: Vec<mpsc::Sender<ProtocolEvent>>,
    metrics_sys: &mut MetricsSystem,
) -> Vec<JoinHandle<()>> {
    assert_eq!(
        worker_rxs.len(),
        event_txs.len(),
        "worker_rxs.len() must equal event_txs.len() (composition-root wiring bug)"
    );

    let mut handles = Vec::with_capacity(worker_rxs.len());
    for (i, (mut wrx, event_tx)) in worker_rxs.into_iter().zip(event_txs).enumerate() {
        let worker_metrics = metrics_sys.register_worker(
            &format!("worker.{i}"),
            &[
                Metric::NetPacketsParsed,
                Metric::HttpRequestsParsed,
                Metric::HttpResponsesParsed,
                Metric::SseEventsParsed,
                Metric::HttpResyncEvents,
                Metric::FlowsTimedOut,
            ],
        );

        handles.push(tokio::spawn(async move {
            let shard = i;
            let mut worker = FlowWorker::new(worker_metrics);
            let reason = 'main: loop {
                let input = match wrx.recv().await {
                    Some(x) => x,
                    None => break 'main "upstream_eof",
                };
                for event in worker.process(input) {
                    if event_tx.send(event).await.is_err() {
                        break 'main "downstream_closed";
                    }
                }
            };
            match reason {
                "upstream_eof" => {
                    tracing::debug!(shard, "protocol worker stopping: upstream EOF");
                }
                r => {
                    tracing::warn!(
                        shard,
                        reason = r,
                        "protocol worker stopping: downstream closed"
                    );
                }
            }
        }));
    }
    handles
}
