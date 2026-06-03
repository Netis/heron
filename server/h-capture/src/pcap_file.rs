use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use pcap::Capture;
use tokio_util::sync::CancellationToken;
use tracing;

use h_common::internal_metrics::{Metric, MetricsWorker};

use crate::packet::RawPacket;
use crate::pcap_dump::{PacketDumper, PacketDumperConfig};
use crate::routing::RoutingSender;
use crate::source::CaptureSource;

/// Gap (µs) inserted between loop iterations' timestamps so that flows from a
/// prior pass exceed the protocol layer's flow-idle timeout (120 s in
/// `h-protocol`) and get reaped — keeping the flow table bounded across a long
/// soak. 5 min > 120 s with margin.
const LOOP_GAP_US: i64 = 300_000_000;

/// Reads packets from a pcap file on a blocking thread.
pub struct PcapFileSource {
    path: PathBuf,
    source_id: String,
    dump_cfg: Option<PacketDumperConfig>,
    /// Replay the file this many times (default 1). >1 enables loop mode.
    loop_count: u32,
    /// Replay until this many seconds elapse (0 = disabled). Takes precedence
    /// over `loop_count` when > 0.
    loop_secs: u64,
    /// Target sustained emission rate in packets/sec (0 = unthrottled = today's
    /// behavior). When > 0, the reader paces packet emission to this aggregate
    /// rate across all passes so a load soak drives a *steady, prod-like* load
    /// instead of an as-fast-as-possible firehose that just saturates the
    /// channels (which only exercises backpressure, not steady-state health).
    rate_pps: u32,
}

impl PcapFileSource {
    pub fn new(path: PathBuf, source_id: String, dump_cfg: Option<PacketDumperConfig>) -> Self {
        Self {
            path,
            source_id,
            dump_cfg,
            loop_count: 1,
            loop_secs: 0,
            rate_pps: 0,
        }
    }

    /// Enable loop/duration replay (for the load/longevity soak). Each pass is
    /// tagged with a fresh per-iteration source_id so its flows are distinct
    /// (driving real full-pipeline + storage load, not deduped retransmits),
    /// and its timestamps are offset so prior-pass flows time out → the flow
    /// table stays bounded. `loop_count` is clamped to ≥ 1.
    pub fn with_loop(mut self, loop_count: u32, loop_secs: u64) -> Self {
        self.loop_count = loop_count.max(1);
        self.loop_secs = loop_secs;
        self
    }

    /// Pace emission to a target aggregate packets/sec (0 = unthrottled).
    /// Pairs with `with_loop` to drive a steady sustained load from a small
    /// corpus for the load/longevity soak.
    pub fn with_rate_pps(mut self, rate_pps: u32) -> Self {
        self.rate_pps = rate_pps;
        self
    }
}

#[async_trait]
impl CaptureSource for PcapFileSource {
    async fn run(
        self: Box<Self>,
        tx: RoutingSender,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        let path = self.path.clone();
        let source_id = self.source_id.clone();
        let dump_cfg = self.dump_cfg.clone();
        let dumper_metrics = metrics.clone();
        let loop_count = self.loop_count.max(1);
        let loop_secs = self.loop_secs;
        let rate_pps = self.rate_pps;
        let looping = loop_secs > 0 || loop_count > 1;

        // Run pcap reading on a blocking thread since next_packet() is blocking.
        let result = tokio::task::spawn_blocking(move || -> crate::Result<()> {
            let mut dumper = dump_cfg.and_then(|c| match PacketDumper::new(c, dumper_metrics) {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!("pcap-file: packet dump disabled: {e}");
                    None
                }
            });

            let start = std::time::Instant::now();
            let mut count: u64 = 0;
            let mut iter: u64 = 0;
            // Span of the first pass, used to space later passes apart in time.
            let mut min_ts: i64 = i64::MAX;
            let mut max_ts: i64 = i64::MIN;

            // Outer loop = one replay pass of the whole file.
            'replay: loop {
                // Fresh per-pass source_id when looping → distinct flows (not
                // deduped as retransmits). Single pass keeps the base id, so
                // non-loop behavior (and stored source_id) is unchanged.
                let pass_source_id = if looping {
                    format!("{source_id}-{iter}")
                } else {
                    source_id.clone()
                };
                // Monotonic offset so a prior pass's flows age out of the flow
                // table. min/max are known after pass 0; pass 0 itself = no
                // offset.
                let offset = if iter == 0 || max_ts < min_ts {
                    0
                } else {
                    (max_ts - min_ts + LOOP_GAP_US).saturating_mul(iter as i64)
                };

                let mut cap = Capture::from_file(&path)?;
                let link_type = cap.get_datalink().0 as u32;
                if iter == 0 {
                    tracing::info!(
                        "pcap-file: opened {} (link_type={}{})",
                        path.display(),
                        link_type,
                        if looping || rate_pps > 0 {
                            format!(
                                ", loop_count={loop_count} loop_secs={loop_secs} rate_pps={rate_pps}"
                            )
                        } else {
                            String::new()
                        }
                    );
                }

                loop {
                    if cancel.is_cancelled() {
                        tracing::debug!("pcap-file: cancellation requested, stopping");
                        break 'replay;
                    }

                    match cap.next_packet() {
                        Ok(packet) => {
                            let ts = packet.header.ts;
                            let raw_ts = ts.tv_sec as i64 * 1_000_000 + ts.tv_usec as i64;
                            if iter == 0 {
                                min_ts = min_ts.min(raw_ts);
                                max_ts = max_ts.max(raw_ts);
                            }

                            let raw = RawPacket {
                                timestamp_us: raw_ts.saturating_add(offset),
                                caplen: packet.header.caplen,
                                wirelen: packet.header.len,
                                link_type,
                                data: Bytes::copy_from_slice(packet.data),
                                source_id: pass_source_id.clone(),
                            };

                            if let Some(d) = dumper.as_mut() {
                                d.write(&raw);
                            }

                            // If the receiver is gone, stop reading.
                            if tx.blocking_send(raw).is_err() {
                                tracing::debug!("pcap-file: channel closed, stopping");
                                break 'replay;
                            }

                            count += 1;
                            metrics.counter(Metric::CapturePacketsReceived).add(1);
                            // Mirror pcap_live's truncation surfacing so that
                            // replaying a snaplen-truncated dump shows the same
                            // signal — not a silent re-emergence of the bug.
                            if packet.header.caplen < packet.header.len {
                                metrics.counter(Metric::CaptureTruncatedPackets).inc();
                            }

                            // Rate pacing: keep cumulative emission on a steady
                            // `rate_pps` schedule. Referencing absolute (count vs
                            // elapsed-since-start) instead of per-packet sleeps
                            // avoids drift and naturally absorbs a slow burst.
                            // Sleep in bounded chunks so cancellation (and a low
                            // rate) stay responsive.
                            if rate_pps > 0 {
                                let target_ns =
                                    count as u128 * 1_000_000_000u128 / rate_pps as u128;
                                let elapsed_ns = start.elapsed().as_nanos();
                                if target_ns > elapsed_ns {
                                    let mut remaining = (target_ns - elapsed_ns) as u64;
                                    while remaining > 0 {
                                        if cancel.is_cancelled() {
                                            break 'replay;
                                        }
                                        let chunk = remaining.min(50_000_000); // ≤50ms
                                        std::thread::sleep(std::time::Duration::from_nanos(
                                            chunk,
                                        ));
                                        remaining -= chunk;
                                    }
                                }
                            }
                        }
                        Err(pcap::Error::NoMorePackets) => {
                            // End of THIS pass.
                            break;
                        }
                        Err(e) => {
                            tracing::error!("pcap-file: read error: {e}");
                            return Err(e.into());
                        }
                    }
                }

                iter += 1;
                // Termination: duration takes precedence over count.
                if loop_secs > 0 {
                    if start.elapsed().as_secs() >= loop_secs {
                        break;
                    }
                } else if iter >= loop_count as u64 {
                    break;
                }
            }

            if let Some(d) = dumper.as_mut() {
                d.flush_all();
            }
            tracing::info!("pcap-file: finished reading {count} packets over {iter} pass(es)");
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(join_err) => Err(crate::CaptureError::Other(format!(
                "pcap-file task panicked: {join_err}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use h_common::internal_metrics::{Metric, MetricsSystem};

    /// Create a MetricsWorker suitable for capture tests.
    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        sys.register_worker(
            "test",
            &[
                Metric::CapturePacketsReceived,
                Metric::CaptureKernelPacketsDropped,
                Metric::CaptureTruncatedPackets,
            ],
        )
    }

    #[tokio::test]
    async fn test_pcap_file_not_found_returns_error() {
        let source = PcapFileSource::new(
            PathBuf::from("/nonexistent/test.pcap"),
            "test".to_string(),
            None,
        );
        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let result = Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await;
        assert!(result.is_err(), "should return error for missing file");
    }

    #[tokio::test]
    async fn test_truncated_pcap_file_returns_error() {
        // Build a pcap file with a packet header claiming 100 bytes but only 10 present.
        let mut data = Vec::new();
        // pcap global header (little-endian, magic 0xa1b2c3d4)
        data.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes()); // version major
        data.extend_from_slice(&4u16.to_le_bytes()); // version minor
        data.extend_from_slice(&0i32.to_le_bytes()); // thiszone
        data.extend_from_slice(&0u32.to_le_bytes()); // sigfigs
        data.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
        data.extend_from_slice(&1u32.to_le_bytes()); // link type (Ethernet)
                                                     // Packet header claiming 100 bytes, but only 10 bytes of data follow
        data.extend_from_slice(&0u32.to_le_bytes()); // ts_sec
        data.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
        data.extend_from_slice(&100u32.to_le_bytes()); // caplen
        data.extend_from_slice(&100u32.to_le_bytes()); // orig_len
        data.extend_from_slice(&[0u8; 10]); // truncated

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.pcap");
        std::fs::write(&path, &data).unwrap();

        let source = PcapFileSource::new(path, "test".to_string(), None);
        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let result = Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await;
        assert!(
            result.is_err(),
            "truncated pcap should return error, not Ok"
        );
    }

    /// Build a minimal valid pcap file with `n` packets.
    fn build_pcap_file(n: u32) -> Vec<u8> {
        let mut data = Vec::new();
        // pcap global header (little-endian)
        data.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&65535u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes()); // Ethernet
                                                     // N packets of 64 bytes each
        let pkt_data = [0u8; 64];
        for i in 0..n {
            data.extend_from_slice(&i.to_le_bytes()); // ts_sec
            data.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
            data.extend_from_slice(&(pkt_data.len() as u32).to_le_bytes());
            data.extend_from_slice(&(pkt_data.len() as u32).to_le_bytes());
            data.extend_from_slice(&pkt_data);
        }
        data
    }

    #[tokio::test]
    async fn test_channel_close_stops_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pcap");
        std::fs::write(&path, build_pcap_file(5)).unwrap();

        // Drop receiver immediately — source should exit Ok.
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let source = PcapFileSource::new(path, "test".to_string(), None);
        let cancel = CancellationToken::new();
        let result = Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await;
        assert!(result.is_ok(), "should return Ok when channel is closed");
    }

    #[tokio::test]
    async fn test_cancellation_stops_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pcap");
        std::fs::write(&path, build_pcap_file(5)).unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        cancel.cancel(); // Pre-cancel

        let source = PcapFileSource::new(path, "test".to_string(), None);
        let result = Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await;
        assert!(result.is_ok(), "should return Ok when pre-cancelled");
    }

    #[tokio::test]
    async fn test_normal_eof_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pcap");
        std::fs::write(&path, build_pcap_file(3)).unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let source = PcapFileSource::new(path, "test".to_string(), None);
        let result = Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await;
        assert!(result.is_ok(), "should return Ok on normal EOF");

        // Drain channel and count packets.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 3, "should have received 3 packets");
    }

    #[tokio::test]
    async fn test_single_pass_keeps_base_source_id() {
        // No loop → original source_id is preserved (backward compatible).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.pcap");
        std::fs::write(&path, build_pcap_file(2)).unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let source = PcapFileSource::new(path, "base".to_string(), None);
        Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await
            .unwrap();

        let mut sids = std::collections::BTreeSet::new();
        while let Ok(pkt) = rx.try_recv() {
            sids.insert(pkt.source_id);
        }
        assert_eq!(sids.into_iter().collect::<Vec<_>>(), vec!["base".to_string()]);
    }

    #[tokio::test]
    async fn test_loop_mode_emits_fresh_flows_per_pass() {
        // Replay a 3-packet file 3× → 9 packets, each pass tagged with a
        // distinct source_id ("base-0/1/2") so downstream sees FRESH flows
        // (FlowKey is keyed by source_id), and timestamps are monotonic across
        // passes so prior-pass flows age out.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loop.pcap");
        std::fs::write(&path, build_pcap_file(3)).unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let source = PcapFileSource::new(path, "base".to_string(), None).with_loop(3, 0);
        let result = Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await;
        assert!(result.is_ok());

        // Group received packets by source_id.
        let mut by_sid: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
        while let Ok(pkt) = rx.try_recv() {
            by_sid.entry(pkt.source_id.clone()).or_default().push(pkt.timestamp_us);
        }
        assert_eq!(
            by_sid.keys().cloned().collect::<Vec<_>>(),
            vec!["base-0".to_string(), "base-1".to_string(), "base-2".to_string()],
            "expected 3 distinct per-pass source_ids"
        );
        for (sid, ts) in &by_sid {
            assert_eq!(ts.len(), 3, "pass {sid} should have 3 packets");
        }
        // Monotonic across passes: pass 1 starts strictly after pass 0 ends.
        let max0 = *by_sid["base-0"].iter().max().unwrap();
        let min1 = *by_sid["base-1"].iter().min().unwrap();
        assert!(min1 > max0, "pass 1 timestamps must start after pass 0 ends");
    }

    #[tokio::test]
    async fn test_loop_secs_zero_count_one_is_single_pass() {
        // with_loop(1, 0) must behave exactly like no loop (single pass, base id).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noloop.pcap");
        std::fs::write(&path, build_pcap_file(4)).unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let source = PcapFileSource::new(path, "base".to_string(), None).with_loop(1, 0);
        Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await
            .unwrap();

        let mut count = 0;
        let mut sids = std::collections::BTreeSet::new();
        while let Ok(pkt) = rx.try_recv() {
            count += 1;
            sids.insert(pkt.source_id);
        }
        assert_eq!(count, 4);
        assert_eq!(sids.into_iter().collect::<Vec<_>>(), vec!["base".to_string()]);
    }

    #[tokio::test]
    async fn test_rate_pps_paces_emission() {
        // 6 packets @ 20 pps → schedule target ≈ 300 ms. Assert a clearly-
        // throttled floor (≥200 ms). Lower-bound only: pacing sleeps can never
        // make the replay FASTER, so this is robust on a loaded runner —
        // unthrottled (rate_pps=0) the same replay finishes in ~1 ms.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rate.pcap");
        std::fs::write(&path, build_pcap_file(6)).unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let source = PcapFileSource::new(path, "base".to_string(), None).with_rate_pps(20);
        let t0 = std::time::Instant::now();
        Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await
            .unwrap();
        let elapsed = t0.elapsed();

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 6, "all packets still delivered under rate control");
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "rate-paced replay should take ≥200ms, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_rate_pps_zero_is_unthrottled() {
        // rate_pps=0 (default) must not pace — functional check that the path
        // is a no-op (all packets delivered, no hang).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("norate.pcap");
        std::fs::write(&path, build_pcap_file(5)).unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let source = PcapFileSource::new(path, "base".to_string(), None).with_rate_pps(0);
        Box::new(source)
            .run(RoutingSender::single(tx), test_metrics(), cancel)
            .await
            .unwrap();

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 5);
    }
}
