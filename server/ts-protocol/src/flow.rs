use tokio::sync::mpsc;

use ts_common::internal_metrics::{Metric, MetricsWorker};

use crate::de;
use crate::de::error::DecodeError;
use crate::net::ParsedPacket;
use ts_capture::RawPacket;

/// Input to a [`FlowWorker`] shard. Packets are routed by flow hash;
/// heartbeats are broadcast to every shard so each worker can advance its
/// own event-time clock (for flow cleanup and downstream fanout).
#[derive(Debug, Clone)]
pub enum WorkerInput {
    Packet(ParsedPacket),
    Heartbeat { ts: i64, source_id: String },
}

/// Parses raw packets and distributes them to workers by flow key hash.
/// Heartbeat sentinel packets (all-zero MACs + `ether_type = 0xFFFF`) are
/// detected via [`RawPacket::is_heartbeat`] and broadcast to every worker.
pub struct FlowDispatcher {
    worker_txs: Vec<mpsc::Sender<WorkerInput>>,
    metrics: MetricsWorker,
}

impl FlowDispatcher {
    pub fn new(worker_txs: Vec<mpsc::Sender<WorkerInput>>, metrics: MetricsWorker) -> Self {
        Self {
            worker_txs,
            metrics,
        }
    }

    /// Handle a single raw packet (or heartbeat sentinel).
    /// Returns false if all worker channels are closed.
    pub async fn dispatch(&self, raw: RawPacket) -> bool {
        if raw.is_heartbeat() {
            self.broadcast_heartbeat(raw.timestamp_us, raw.source_id);
            return true;
        }
        self.dispatch_packet(&raw).await
    }

    async fn dispatch_packet(&self, raw: &RawPacket) -> bool {
        let parsed = match de::decode(
            &raw.data,
            raw.link_type,
            raw.timestamp_us,
            raw.source_id.clone(),
        ) {
            Ok(p) => p,
            Err(DecodeError::NotIp) | Err(DecodeError::NotSupported) => {
                self.metrics.counter(Metric::NetParseDroppedNotIp).inc();
                return true;
            }
            Err(DecodeError::NotTcp) => {
                self.metrics.counter(Metric::NetParseDroppedNotTcp).inc();
                return true;
            }
            Err(DecodeError::Truncated) | Err(DecodeError::InvalidHeader) => {
                self.metrics.counter(Metric::NetParseDroppedMalformed).inc();
                return true;
            }
        };

        self.metrics.counter(Metric::DispatcherPacketsRouted).inc();

        let worker_idx = (parsed.flow_key.shard_hash() as usize) % self.worker_txs.len();
        self.worker_txs[worker_idx]
            .send(WorkerInput::Packet(parsed))
            .await
            .is_ok()
    }

    /// Fan out a heartbeat to every worker. Uses `try_send` to avoid
    /// head-of-line blocking if one shard is momentarily full — dropping a
    /// heartbeat only delays that shard's sweep by one interval, which is
    /// acceptable.
    fn broadcast_heartbeat(&self, ts: i64, source_id: String) {
        for tx in &self.worker_txs {
            if tx
                .try_send(WorkerInput::Heartbeat {
                    ts,
                    source_id: source_id.clone(),
                })
                .is_err()
            {
                self.metrics
                    .counter(Metric::FlowHeartbeatsDropped)
                    .inc();
            }
        }
    }
}
