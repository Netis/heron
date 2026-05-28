use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use pcap::{Capture, Device};
use tokio_util::sync::CancellationToken;
use tracing;

use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_common::source_registry::{self, SourceRegistry};
use ts_common::throttle::ThrottledWarn;

const ERR_WARN_THROTTLE: Duration = Duration::from_secs(5);
const ERR_BACKOFF: Duration = Duration::from_millis(50);

use crate::heartbeat::{HEARTBEAT_INTERVAL_US, SAFETY_MARGIN_US};
use crate::packet::RawPacket;
use crate::pcap_dump::{PacketDumper, PacketDumperConfig};
use crate::routing::RoutingSender;
use crate::source::CaptureSource;

/// Captures live packets from a network interface via libpcap.
///
/// Runs `next_packet()` on a blocking thread with a read timeout so the loop
/// can detect channel closure and cancellation (graceful shutdown).
///
/// Synthesizes sentinel heartbeat packets via [`RawPacket::heartbeat`] on a
/// fixed 1 s cadence so downstream stages driven by packet timestamps (TCP
/// cleanup, turn sweep, metrics window close) keep advancing. A single
/// `last_hb_ts` tracks the most-recent HB / baseline time; both triggers
/// below read and update it:
/// * real-packet path — when `pkt.ts - last_hb_ts >= HEARTBEAT_INTERVAL_US`,
///   stamp the HB with the real packet's kernel timestamp (monotonic).
/// * idle-fallback — on read timeout, when `wall_clock_us() - last_hb_ts
///   >= HEARTBEAT_INTERVAL_US`, stamp the HB with `wall_clock_us() -
///   SAFETY_MARGIN_US`, clamped above `last_hb_ts`, so an imminent real
///   packet cannot race ahead. pcap kernel timestamps and `wall_clock_us`
///   share `CLOCK_REALTIME`, so the comparison is meaningful.
pub struct PcapLiveSource {
    interface: String,
    bpf_filter: Option<String>,
    snaplen: u32,
    stream_id: String,
    dump_cfg: Option<PacketDumperConfig>,
    registry: Arc<SourceRegistry>,
}

impl PcapLiveSource {
    pub fn new(
        interface: String,
        bpf_filter: Option<String>,
        snaplen: u32,
        stream_id: String,
        dump_cfg: Option<PacketDumperConfig>,
        registry: Arc<SourceRegistry>,
    ) -> Self {
        Self {
            interface,
            bpf_filter,
            snaplen,
            stream_id,
            dump_cfg,
            registry,
        }
    }
}

#[async_trait]
impl CaptureSource for PcapLiveSource {
    async fn run(
        self: Box<Self>,
        tx: RoutingSender,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        let interface = self.interface.clone();
        let bpf_filter = self.bpf_filter.clone();
        let snaplen = self.snaplen;
        let stream_id = self.stream_id.clone();
        let dump_cfg = self.dump_cfg.clone();
        let dumper_metrics = metrics.clone();
        let registry = self.registry.clone();

        let result = tokio::task::spawn_blocking(move || -> crate::Result<()> {
            // Best-effort: open the packet dumper if configured. A failure to
            // create the output directory logs + disables dumping for this
            // source; capture itself continues.
            let mut dumper = dump_cfg.and_then(|c| match PacketDumper::new(c, dumper_metrics) {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!("pcap-live: packet dump disabled: {e}");
                    None
                }
            });
            // Find the device by name.
            let device = Device::list()?
                .into_iter()
                .find(|d| d.name == interface)
                .ok_or_else(|| {
                    crate::CaptureError::Other(format!("interface '{}' not found", interface))
                })?;

            let mut cap = Capture::from_device(device)?
                .immediate_mode(true)
                .snaplen(snaplen as i32)
                .timeout(500) // 500ms read timeout for shutdown responsiveness
                .open()?;

            if let Some(ref filter) = bpf_filter {
                cap.filter(filter, true)?;
                tracing::info!("pcap-live: BPF filter set: {}", filter);
            }

            let link_type = cap.get_datalink().0 as u32;
            tracing::info!(
                "pcap-live: capturing on {} (link_type={}, snaplen={})",
                interface,
                link_type,
                snaplen,
            );

            let mut count: u64 = 0;
            let mut last_dropped: u64 = 0;
            let mut last_hb_ts: i64 = 0;
            let mut err_throttle = ThrottledWarn::new(ERR_WARN_THROTTLE);

            // Sample pcap stats and update the dropped-packets metric.
            // Called on every timeout (~500ms) and at exit.
            let update_drop_stats = |cap: &mut Capture<pcap::Active>,
                                     last_dropped: &mut u64,
                                     metrics: &MetricsWorker| {
                if let Ok(stats) = cap.stats() {
                    let dropped = stats.dropped as u64;
                    if dropped > *last_dropped {
                        metrics
                            .counter(Metric::CapturePacketsDropped)
                            .add(dropped - *last_dropped);
                        *last_dropped = dropped;
                    }
                }
            };

            loop {
                if cancel.is_cancelled() {
                    tracing::debug!("pcap-live: cancellation requested, stopping");
                    break;
                }

                match cap.next_packet() {
                    Ok(packet) => {
                        let ts = packet.header.ts;
                        let timestamp_us =
                            ts.tv_sec as i64 * 1_000_000 + ts.tv_usec as i64;

                        let raw = RawPacket {
                            timestamp_us,
                            caplen: packet.header.caplen,
                            wirelen: packet.header.len,
                            link_type,
                            data: Bytes::copy_from_slice(packet.data),
                            stream_id: stream_id.clone(),
                        };

                        // Packet-driven heartbeat: if event-time has advanced a
                        // full interval since the last HB, emit one stamped at
                        // the real packet's kernel timestamp. This keeps
                        // metric/turn windows closing during long SSE streams.
                        if last_hb_ts == 0 {
                            last_hb_ts = raw.timestamp_us;
                        } else if raw.timestamp_us - last_hb_ts >= HEARTBEAT_INTERVAL_US {
                            let hb = RawPacket::heartbeat(raw.timestamp_us, stream_id.clone());
                            if tx.blocking_send(hb).is_err() {
                                tracing::debug!("pcap-live: channel closed, stopping");
                                break;
                            }
                            metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                            registry.touch(&stream_id, source_registry::now_ms(), true);
                            last_hb_ts = raw.timestamp_us;
                        }

                        if let Some(d) = dumper.as_mut() {
                            d.write(&raw);
                        }

                        if tx.blocking_send(raw).is_err() {
                            tracing::debug!("pcap-live: channel closed, stopping");
                            break;
                        }

                        count += 1;
                        metrics.counter(Metric::CapturePacketsReceived).inc();
                        registry.touch(&stream_id, source_registry::now_ms(), false);
                    }
                    Err(pcap::Error::TimeoutExpired) => {
                        // Periodically update drop stats during idle.
                        update_drop_stats(&mut cap, &mut last_dropped, &metrics);

                        if cancel.is_cancelled() || tx.is_closed() {
                            tracing::debug!("pcap-live: shutdown during timeout, stopping");
                            break;
                        }

                        // Idle fallback: if a full interval of wall time has
                        // elapsed since the last HB / baseline, emit one.
                        // Guard `last_hb_ts > 0` keeps us silent on an
                        // interface that has never seen a packet. Stamp the
                        // HB slightly in the past and clamp above
                        // `last_hb_ts` for strict monotonicity.
                        if last_hb_ts > 0 {
                            let wall_us = wall_clock_us();
                            if wall_us - last_hb_ts >= HEARTBEAT_INTERVAL_US {
                                let hb_ts = (wall_us - SAFETY_MARGIN_US).max(last_hb_ts + 1);
                                let hb = RawPacket::heartbeat(hb_ts, stream_id.clone());
                                if tx.blocking_send(hb).is_err() {
                                    tracing::debug!("pcap-live: channel closed, stopping");
                                    break;
                                }
                                metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                                registry.touch(&stream_id, source_registry::now_ms(), true);
                                last_hb_ts = hb_ts;
                            }
                        }

                        continue;
                    }
                    Err(e) => {
                        // Non-fatal: a single libpcap read can fail transiently
                        // (kernel buffer full, signal interrupted, etc). Counting
                        // the error and backing off beats tearing down the whole
                        // source — operators see the counter climb and the
                        // throttled warn shows the latest error string.
                        metrics.counter(Metric::CaptureSourceErrors).inc();
                        if let Some(suppressed) = err_throttle.tick() {
                            if suppressed > 0 {
                                tracing::warn!(
                                    suppressed,
                                    "pcap-live: capture error (latest of many): {e}"
                                );
                            } else {
                                tracing::warn!("pcap-live: capture error: {e}");
                            }
                        }
                        std::thread::sleep(ERR_BACKOFF);
                        continue;
                    }
                }
            }

            // Final stats update and log.
            update_drop_stats(&mut cap, &mut last_dropped, &metrics);
            // Explicit flush before Drop as defense-in-depth: if the runtime
            // aborts this task without running Drop, the on-disk pcap file is
            // still well-formed up to the last completed packet.
            if let Some(d) = dumper.as_mut() {
                d.flush_all();
            }
            if let Ok(stats) = cap.stats() {
                tracing::info!(
                    "pcap-live: stopped after {} packets (pcap stats: received={}, dropped={}, if_dropped={})",
                    count, stats.received, stats.dropped, stats.if_dropped,
                );
            } else {
                tracing::info!("pcap-live: stopped after {} packets", count);
            }

            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(join_err) => Err(crate::CaptureError::Other(format!(
                "pcap-live task panicked: {join_err}"
            ))),
        }
    }
}

fn wall_clock_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
