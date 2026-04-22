use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ts_common::internal_metrics::MetricsWorker;

use crate::routing::RoutingSender;

/// Unified interface for all capture sources (pcap, pcap-file, cloud-probe).
///
/// Each source runs as a long-lived task, pushing [`RawPacket`]s into a
/// [`RoutingSender`] which transparently routes packets to one of N dispatcher
/// channels by `hash(source_id)`. When there is only one dispatcher the
/// routing is a no-op. Heartbeats are emitted as sentinel `RawPacket`s
/// (all-zero MACs + `ether_type = 0xFFFF`) — see [`RawPacket::is_heartbeat`].
/// The source is consumed when `run()` is called.
#[async_trait]
pub trait CaptureSource: Send {
    /// Run the capture source, sending events to `tx`.
    ///
    /// Returns `Ok(())` when:
    /// - The source is exhausted (e.g., end of pcap file)
    /// - The `cancel` token is triggered
    /// - The channel `tx` is closed
    ///
    /// Returns `Err` on unrecoverable errors.
    async fn run(
        self: Box<Self>,
        tx: RoutingSender,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()>;
}
