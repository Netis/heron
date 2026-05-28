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
//!   consuming `WorkerInput`s and producing `HttpParseEvent`s.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use h_capture::RawPacket;
use h_common::internal_metrics::{Metric, MetricsSystem};

use crate::flow::{FlowDispatcher, WorkerInput};
use crate::joiner::{HttpExchange, HttpJoiner, HttpJoinerEvent};
use crate::model::HttpParseEvent;
use crate::tcp::FlowWorker;

/// Spawn the flow dispatcher: one task that consumes `RawPacket`s from
/// `capture_rx` and routes each to `worker_txs`. Real packets are sharded
/// by flow hash; heartbeat sentinels are broadcast to every shard.
///
/// When a pipeline has multiple dispatchers, the composition root creates
/// one channel per dispatcher and wraps the senders in a `RoutingSender`
/// that routes by `hash(source_id)`. Each dispatcher receives its own
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
            Metric::FlowHeartbeatsDropped,
            Metric::NetParseDroppedNotIp,
            Metric::NetParseDroppedNotTcp,
            Metric::NetParseDroppedMalformed,
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
/// parsing, and emits `HttpParseEvent`s into `event_txs[i]`.
///
/// Panics if `worker_rxs.len() != event_txs.len()` — that is a wiring bug
/// in the composition root, not a runtime condition.
pub fn spawn_protocol_stage(
    worker_rxs: Vec<mpsc::Receiver<WorkerInput>>,
    event_txs: Vec<mpsc::Sender<HttpParseEvent>>,
    metrics_sys: &mut MetricsSystem,
) -> Vec<JoinHandle<()>> {
    assert_eq!(
        worker_rxs.len(),
        event_txs.len(),
        "worker_rxs.len() must equal event_txs.len() (composition-root wiring bug)"
    );

    // Per-shard gauges updated after every `FlowWorker::process` call. Probe
    // sums across shards: total active flows is the operationally useful
    // number; per-shard skew is a separate concern.
    let flow_gauges: Vec<Arc<AtomicU64>> = (0..worker_rxs.len())
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    let gauges_for_probe = flow_gauges.clone();
    metrics_sys.register_queue_probe(Metric::FlowsActive, move || {
        gauges_for_probe
            .iter()
            .map(|g| g.load(Ordering::Relaxed))
            .sum()
    });

    let mut handles = Vec::with_capacity(worker_rxs.len());
    for (i, ((mut wrx, event_tx), flow_gauge)) in worker_rxs
        .into_iter()
        .zip(event_txs)
        .zip(flow_gauges.into_iter())
        .enumerate()
    {
        let worker_metrics = metrics_sys.register_worker(
            &format!("worker.{i}"),
            &[
                Metric::NetPacketsParsed,
                Metric::HttpParseReq,
                Metric::HttpParseResp,
                Metric::SseEventsParsed,
                Metric::HttpResyncEvents,
                Metric::TcpOutOfOrderDrops,
                Metric::TcpOutOfOrderBuffered,
                Metric::TcpRetransmissionsIgnored,
                Metric::FlowsExpired,
                Metric::FlowHeartbeatsReceived,
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
                flow_gauge.store(worker.flow_count() as u64, Ordering::Relaxed);
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

/// Spawn N HttpJoiner workers, one per input receiver. Each worker pairs
/// `HttpParseEvent`s into `HttpJoinerEvent`s (RequestObserved + Exchange +
/// Heartbeat) and emits them to `joiner_event_txs[i]`.
///
/// When `http_exchanges_tx` is `Some`, every paired `Exchange`'s
/// `HttpExchange` is additionally forwarded to the shared storage channel —
/// this is how non-LLM traffic reaches `http_exchanges`.
///
/// **Backpressure coupling.** For each paired exchange the worker sends to
/// `http_exchanges_tx` before `joiner_event_txs[i]`. If the exchanges sink
/// saturates, the LLM pipeline on that shard stalls behind it. This is
/// observable via `StorageQueueDepthHttpExchanges`; use that metric to detect
/// storage-induced stalls. The ordering was chosen to keep storage
/// authoritative for raw transport records — a dropped storage send
/// followed by a successful LLM send would lose the ground-truth row.
///
/// Panics if `worker_rxs.len() != joiner_event_txs.len()` — that is a
/// wiring bug in the composition root.
pub fn spawn_http_joiner_stage(
    worker_rxs: Vec<mpsc::Receiver<HttpParseEvent>>,
    joiner_event_txs: Vec<mpsc::Sender<HttpJoinerEvent>>,
    http_exchanges_tx: Option<mpsc::Sender<HttpExchange>>,
    metrics_sys: &mut MetricsSystem,
) -> Vec<JoinHandle<()>> {
    assert_eq!(
        worker_rxs.len(),
        joiner_event_txs.len(),
        "worker_rxs.len() must equal joiner_event_txs.len() (composition-root wiring bug)"
    );

    // Per-shard gauges for HttpJoiner.pending HashMap size. Same pattern as
    // FlowsActive above: probe sums across shards. The pending HashMap is
    // unbounded in pathological flows (requests without matching responses),
    // so this is a leading OOM signal.
    let pending_gauges: Vec<Arc<AtomicU64>> = (0..worker_rxs.len())
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    let gauges_for_probe = pending_gauges.clone();
    metrics_sys.register_queue_probe(Metric::HttpJoinerPending, move || {
        gauges_for_probe
            .iter()
            .map(|g| g.load(Ordering::Relaxed))
            .sum()
    });

    let mut handles = Vec::with_capacity(worker_rxs.len());
    for (i, ((mut wrx, ev_tx), pending_gauge)) in worker_rxs
        .into_iter()
        .zip(joiner_event_txs)
        .zip(pending_gauges.into_iter())
        .enumerate()
    {
        let worker_metrics = metrics_sys.register_worker(
            &format!("joiner.{i}"),
            &[
                Metric::HttpJoinerDone,
                Metric::HttpJoinerUnpaired,
                Metric::HttpJoinerExpired,
                Metric::JoinerHeartbeatsReceived,
            ],
        );
        let exch_tx = http_exchanges_tx.clone();

        handles.push(tokio::spawn(async move {
            let shard = i;
            let mut joiner = HttpJoiner::new(worker_metrics);
            let reason = 'main: loop {
                let input = match wrx.recv().await {
                    Some(x) => x,
                    None => break 'main "upstream_eof",
                };
                for event in joiner.process(input) {
                    // Fan out the storage-bound slice first — constructing
                    // the storage `HttpExchange` is just Arc::clone (no
                    // header/body deep copies).
                    if let (
                        HttpJoinerEvent::Exchange {
                            id,
                            request,
                            response,
                            sse_events,
                        },
                        Some(tx),
                    ) = (&event, exch_tx.as_ref())
                    {
                        let (sse_event_count, sse_data_bytes) =
                            crate::joiner::sse_summary(sse_events);
                        let xchg = HttpExchange {
                            id: id.clone(),
                            request: request.clone(),
                            response: response.clone(),
                            sse_event_count,
                            sse_data_bytes,
                        };
                        if tx.send(xchg).await.is_err() {
                            break 'main "downstream_closed_exchanges";
                        }
                    }
                    if ev_tx.send(event).await.is_err() {
                        break 'main "downstream_closed_joiner";
                    }
                }
                pending_gauge.store(joiner.pending_count() as u64, Ordering::Relaxed);
            };
            match reason {
                "upstream_eof" => {
                    tracing::debug!(shard, "joiner worker stopping: upstream EOF");
                }
                r => {
                    tracing::warn!(
                        shard,
                        reason = r,
                        "joiner worker stopping: downstream closed"
                    );
                }
            }
        }));
    }
    handles
}
