use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use pcap::{Capture, Device};
use tokio_util::sync::CancellationToken;
use tracing;

use h_common::internal_metrics::{Metric, MetricsWorker};
use h_common::throttle::ThrottledWarn;

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
    source_id: String,
    dump_cfg: Option<PacketDumperConfig>,
}

impl PcapLiveSource {
    pub fn new(
        interface: String,
        bpf_filter: Option<String>,
        snaplen: u32,
        source_id: String,
        dump_cfg: Option<PacketDumperConfig>,
    ) -> Self {
        Self {
            interface,
            bpf_filter,
            snaplen,
            source_id,
            dump_cfg,
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
        let source_id = self.source_id.clone();
        let dump_cfg = self.dump_cfg.clone();
        let dumper_metrics = metrics.clone();

        // Hand the pcap BreakLoop handle off to an async waker as soon as the
        // capture is open. `pcap_next_ex` on a live capture can sit in a
        // kernel read past the configured timeout on some platforms (notably
        // macOS BPF on quiet interfaces); without this, cancel can be
        // observed only at the NEXT return from next_packet, which may never
        // come. When cancel fires the waker calls `pcap_breakloop`, forcing
        // `next_packet` to surface as `TimeoutExpired`, where the loop
        // re-checks cancel and exits.
        let (breaker_tx, breaker_rx) = tokio::sync::oneshot::channel::<pcap::BreakLoop>();
        let cancel_for_waker = cancel.clone();
        let waker_done = CancellationToken::new();
        let waker_done_inner = waker_done.clone();
        let waker_task = tokio::spawn(async move {
            let breaker = match breaker_rx.await {
                Ok(b) => b,
                Err(_) => return, // capture thread dropped sender before opening
            };
            tokio::select! {
                _ = cancel_for_waker.cancelled() => {
                    tracing::debug!("pcap-live: cancel observed; invoking pcap_breakloop");
                    breaker.breakloop();
                }
                _ = waker_done_inner.cancelled() => {
                    // Capture already exited cleanly; nothing to wake.
                }
            }
        });

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

            // 16 MiB AF_PACKET ring. The libpcap default (~2 MiB) is too
            // small to absorb bursts of GRO-coalesced super-segments
            // (10–60 KB each) that Linux generates on `-i any` for TCP flows
            // carrying large LLM prompts/SSE bodies. Side-effect of an
            // undersized ring is silent loss that does NOT show up in
            // `pcap_stats().dropped` under TPACKET_V3 + immediate_mode,
            // because dropped packets never reach the per-socket ring.
            // 16 MiB matches what `tcpdump -B 16384` users typically pick
            // for chatty TLS / SSE workloads.
            let mut cap = Capture::from_device(device)?
                .immediate_mode(true)
                .snaplen(snaplen as i32)
                .buffer_size(16 * 1024 * 1024)
                .timeout(500) // 500ms read timeout for shutdown responsiveness
                .open()?;

            // Hand the breakloop handle off so the async waker can signal
            // `pcap_next_ex` to return on cancel. We ignore send errors — if
            // the receiver is gone the waker already decided it doesn't need
            // the handle.
            let _ = breaker_tx.send(cap.breakloop_handle());

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
                            .counter(Metric::CaptureKernelPacketsDropped)
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
                            source_id: source_id.clone(),
                            process: None,
                        };

                        // Packet-driven heartbeat: if event-time has advanced a
                        // full interval since the last HB, emit one stamped at
                        // the real packet's kernel timestamp. This keeps
                        // metric/turn windows closing during long SSE streams.
                        if last_hb_ts == 0 {
                            last_hb_ts = raw.timestamp_us;
                        } else if raw.timestamp_us - last_hb_ts >= HEARTBEAT_INTERVAL_US {
                            let hb = RawPacket::heartbeat(raw.timestamp_us, source_id.clone());
                            if tx.blocking_send(hb).is_err() {
                                tracing::debug!("pcap-live: channel closed, stopping");
                                break;
                            }
                            metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                            last_hb_ts = raw.timestamp_us;
                            // Flush dump buffers on each heartbeat so a hard
                            // termination loses at most ~1s of buffered data.
                            if let Some(d) = dumper.as_mut() {
                                d.flush_all();
                            }
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
                        // Snaplen-truncation surfaces here: when
                        // `caplen < wirelen`, libpcap delivered only the
                        // leading `caplen` bytes of the on-wire frame and
                        // the rest is gone. On `lo` with TSO/GSO this can
                        // happen for super-frames > snaplen even when no
                        // kernel drops are reported. Counter exposes it so
                        // operators can distinguish truncation from real
                        // loss without reading hex dumps.
                        if packet.header.caplen < packet.header.len {
                            metrics.counter(Metric::CaptureTruncatedPackets).inc();
                        }
                    }
                    Err(pcap::Error::NoMorePackets) => {
                        // `pcap_next_ex` returns PCAP_ERROR_BREAK (-2) after
                        // `pcap_breakloop` on a live capture — pcap-rs maps
                        // that to `NoMorePackets`. Treat it as a clean exit
                        // so the shutdown path does not count it as a source
                        // error.
                        tracing::debug!("pcap-live: breakloop observed, stopping");
                        break;
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
                                let hb = RawPacket::heartbeat(hb_ts, source_id.clone());
                                if tx.blocking_send(hb).is_err() {
                                    tracing::debug!("pcap-live: channel closed, stopping");
                                    break;
                                }
                                metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                                last_hb_ts = hb_ts;
                                // Flush dump buffers on each idle heartbeat too.
                                if let Some(d) = dumper.as_mut() {
                                    d.flush_all();
                                }
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
                        metrics.counter(Metric::CaptureReadErrors).inc();
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

        // Release the waker whether the capture exited cleanly or errored.
        // If cancel already won, the waker has already called breakloop()
        // and returned; `waker_done.cancel()` is a no-op on that branch.
        waker_done.cancel();
        let _ = waker_task.await;

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
