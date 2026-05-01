//! Cloud-probe capture source.
//!
//! Receives batches of packets over a ZMQ `PULL` socket from remote
//! cloud-probe instances. Each batch carries a 24-byte header followed by
//! `pkts_num` per-packet records (wire format documented in
//! `docs/design/capture.md`). Batch-level metadata (uuid, service_tag,
//! keybit) is currently discarded — only the packet bytes and timestamps
//! propagate downstream.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zeromq::{PullSocket, Socket, SocketRecv, ZmqMessage};

use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_common::throttle::ThrottledWarn;

use crate::heartbeat::HeartbeatTracker;
use crate::packet::RawPacket;
use crate::pcap_dump::{PacketDumper, PacketDumperConfig};
use crate::routing::RoutingSender;
use crate::source::CaptureSource;

const WARN_THROTTLE: Duration = Duration::from_secs(5);
const ERR_BACKOFF: Duration = Duration::from_millis(50);

const BATCH_HDR_LEN: usize = 24;
const PKT_HDR_LEN: usize = 16;
const PKT_DATA_LEN_FIELD: usize = 2;

/// Link-type cloud-probe always uses at the Ethernet layer.
/// Matches `ts-protocol::de::headers::LINKTYPE_ETHERNET` (value 1).
const LINKTYPE_ETHERNET: u32 = 1;

/// Failure encountered while parsing a ZMQ batch payload. On any failure the
/// caller drops the entire batch (see design doc §Error Handling).
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum BatchError {
    #[error("batch truncated: needed {needed} more bytes at offset {offset}, have {have}")]
    Truncated {
        needed: usize,
        have: usize,
        offset: usize,
    },
}

/// Parse a single ZMQ batch message into a vector of RawPackets.
///
/// Does NOT validate the `version` field: per design decision we accept any
/// batch whose length arithmetic is self-consistent.
pub(crate) fn parse_batch(bytes: &[u8]) -> Result<(String, Vec<RawPacket>), BatchError> {
    let total = bytes.len();
    let mut offset = 0usize;

    if total < BATCH_HDR_LEN {
        return Err(BatchError::Truncated {
            needed: BATCH_HDR_LEN - total,
            have: total,
            offset,
        });
    }

    let uuid = format_uuid(&bytes[8..24]);
    let pkts_num = u16::from_be_bytes([bytes[2], bytes[3]]);
    offset += BATCH_HDR_LEN;

    let mut out = Vec::with_capacity(pkts_num as usize);
    for _ in 0..pkts_num {
        let record_header_len = PKT_DATA_LEN_FIELD + PKT_HDR_LEN;
        if total - offset < record_header_len {
            return Err(BatchError::Truncated {
                needed: record_header_len - (total - offset),
                have: total - offset,
                offset,
            });
        }

        let pkt_data_len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += PKT_DATA_LEN_FIELD;

        let tv_sec = read_u32_be(bytes, offset);
        let tv_usec = read_u32_be(bytes, offset + 4);
        let caplen = read_u32_be(bytes, offset + 8);
        let wirelen = read_u32_be(bytes, offset + 12);
        offset += PKT_HDR_LEN;

        if total - offset < pkt_data_len {
            return Err(BatchError::Truncated {
                needed: pkt_data_len - (total - offset),
                have: total - offset,
                offset,
            });
        }

        let data = Bytes::copy_from_slice(&bytes[offset..offset + pkt_data_len]);
        offset += pkt_data_len;

        out.push(RawPacket {
            timestamp_us: tv_sec as i64 * 1_000_000 + tv_usec as i64,
            caplen,
            wirelen,
            link_type: LINKTYPE_ETHERNET,
            data,
            source_id: String::new(),
        });
    }

    Ok((uuid, out))
}

fn format_uuid(bytes: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

#[inline]
fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

pub struct CloudProbeSource {
    endpoint: String,
    recv_hwm: i32,
    dump_cfg: Option<PacketDumperConfig>,
}

impl CloudProbeSource {
    pub fn new(endpoint: String, recv_hwm: i32, dump_cfg: Option<PacketDumperConfig>) -> Self {
        Self {
            endpoint,
            recv_hwm,
            dump_cfg,
        }
    }
}

#[async_trait]
impl CaptureSource for CloudProbeSource {
    async fn run(
        self: Box<Self>,
        tx: RoutingSender,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        let endpoint = self.endpoint.clone();
        let recv_hwm = self.recv_hwm;

        // Best-effort dumper setup; failure here only disables dumping.
        let mut dumper =
            self.dump_cfg
                .clone()
                .and_then(|c| match PacketDumper::new(c, metrics.clone()) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        tracing::warn!("cloud-probe: packet dump disabled: {e}");
                        None
                    }
                });

        // zmq.rs 0.4 does not expose RCVHWM as a socket option; backpressure
        // flows naturally through the downstream mpsc channel instead. We
        // still surface the configured value in logs so operators can see it.
        let mut socket = PullSocket::new();
        socket.bind(&endpoint).await?;
        tracing::info!(
            "cloud-probe: listening on {} (recv_hwm={} advisory)",
            endpoint,
            recv_hwm,
        );

        let mut batch_count: u64 = 0;
        let mut pkt_count: u64 = 0;
        let mut hb_count: u64 = 0;
        let mut batch_err_throttle = ThrottledWarn::new(WARN_THROTTLE);
        let mut recv_err_throttle = ThrottledWarn::new(WARN_THROTTLE);
        let mut tracker = HeartbeatTracker::new();

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::debug!("cloud-probe: cancellation requested, stopping");
                    break;
                }
                msg = socket.recv() => {
                    match msg {
                        Ok(msg) => {
                            let bytes = flatten_message(&msg);
                            match parse_batch(&bytes) {
                                Ok((uuid, pkts)) => {
                                    metrics.counter(Metric::CaptureBatchesReceived).inc();
                                    batch_count += 1;
                                    for mut pkt in pkts {
                                        pkt.source_id = uuid.clone();
                                        let is_hb = pkt.is_heartbeat();

                                        // Synthesize per-uuid HB if interval
                                        // elapsed. Forwarded *before* the real
                                        // packet so the sequence stays time-
                                        // ordered downstream.
                                        if let Some(hb) = tracker.on_packet(&pkt) {
                                            if tx.send(hb).await.is_err() {
                                                tracing::debug!(
                                                    "cloud-probe: channel closed, stopping"
                                                );
                                                if let Some(d) = dumper.as_mut() {
                                                    d.flush_all();
                                                }
                                                log_summary(
                                                    &endpoint, batch_count, pkt_count, hb_count,
                                                );
                                                return Ok(());
                                            }
                                            metrics
                                                .counter(Metric::CaptureHeartbeatsEmitted)
                                                .inc();
                                            hb_count += 1;
                                            // Flush dump buffers on each synthesized heartbeat so
                                            // a hard termination loses at most ~1s of buffered
                                            // data — same contract as pcap_live.rs's
                                            // heartbeat-driven flush.
                                            if let Some(d) = dumper.as_mut() {
                                                d.flush_all();
                                            }
                                        }

                                        if let Some(d) = dumper.as_mut() {
                                            d.write(&pkt);
                                        }

                                        // Read truncation flag before `pkt` is
                                        // moved into the channel. `caplen <
                                        // wirelen` mirrors the pcap_live
                                        // surfacing — a remote probe whose
                                        // snaplen is smaller than its link's
                                        // max frame produces the same
                                        // tail-truncation wedge as a local lo
                                        // capture.
                                        let truncated = pkt.caplen < pkt.wirelen;

                                        if tx.send(pkt).await.is_err() {
                                            tracing::debug!(
                                                "cloud-probe: channel closed, stopping"
                                            );
                                            if let Some(d) = dumper.as_mut() {
                                                d.flush_all();
                                            }
                                            log_summary(
                                                &endpoint, batch_count, pkt_count, hb_count,
                                            );
                                            return Ok(());
                                        }
                                        if is_hb {
                                            metrics
                                                .counter(Metric::CaptureHeartbeatsEmitted)
                                                .inc();
                                            hb_count += 1;
                                            // Flush dump buffers on upstream heartbeats too —
                                            // matches the synthesized-HB cadence above.
                                            if let Some(d) = dumper.as_mut() {
                                                d.flush_all();
                                            }
                                        } else {
                                            metrics
                                                .counter(Metric::CapturePacketsReceived)
                                                .inc();
                                            if truncated {
                                                metrics
                                                    .counter(Metric::CaptureTruncatedPackets)
                                                    .inc();
                                            }
                                            pkt_count += 1;
                                        }
                                    }
                                }
                                Err(err) => {
                                    metrics.counter(Metric::CaptureZmqBatchesDropped).inc();
                                    if let Some(suppressed) = batch_err_throttle.tick() {
                                        if suppressed > 0 {
                                            tracing::warn!(
                                                suppressed,
                                                bytes = bytes.len(),
                                                "cloud-probe: dropping malformed batch (latest of many): {err}"
                                            );
                                        } else {
                                            tracing::warn!(
                                                bytes = bytes.len(),
                                                "cloud-probe: dropping malformed batch: {err}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            // Non-fatal: keep the socket alive and count the
                            // error. zmq.rs recv on a bound PullSocket rarely
                            // errors, but if it does, tearing down the source
                            // would take the whole pipeline with it. The cancel
                            // token is the only path out.
                            metrics.counter(Metric::CaptureReadErrors).inc();
                            if let Some(suppressed) = recv_err_throttle.tick() {
                                if suppressed > 0 {
                                    tracing::warn!(
                                        suppressed,
                                        "cloud-probe: recv error (latest of many): {err}"
                                    );
                                } else {
                                    tracing::warn!("cloud-probe: recv error: {err}");
                                }
                            }
                            tokio::time::sleep(ERR_BACKOFF).await;
                        }
                    }
                }
            }
        }

        if let Some(d) = dumper.as_mut() {
            d.flush_all();
        }
        log_summary(&endpoint, batch_count, pkt_count, hb_count);
        Ok(())
    }
}

/// Concatenate the frames of a ZMQ message. cloud-probe always sends a
/// single-frame batch, but zmq.rs surfaces the message as a sequence of
/// `Bytes` frames, so we flatten defensively.
fn flatten_message(msg: &ZmqMessage) -> Vec<u8> {
    let total: usize = msg.iter().map(|b| b.len()).sum();
    let mut v = Vec::with_capacity(total);
    for frame in msg.iter() {
        v.extend_from_slice(frame);
    }
    v
}

fn log_summary(endpoint: &str, batches: u64, packets: u64, heartbeats: u64) {
    tracing::info!(
        "cloud-probe: stopped {} (batches={}, packets={}, heartbeats={})",
        endpoint,
        batches,
        packets,
        heartbeats,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a batch-header blob with the given packet count.
    fn batch_header(pkts_num: u16) -> Vec<u8> {
        let mut v = Vec::with_capacity(BATCH_HDR_LEN);
        v.extend_from_slice(&2u16.to_be_bytes()); // version
        v.extend_from_slice(&pkts_num.to_be_bytes()); // pkts_num
        v.extend_from_slice(&0u32.to_be_bytes()); // keybit
        v.extend_from_slice(&[0u8; 16]); // uuid
        v
    }

    /// Append one packet record with the given timestamp and payload bytes.
    /// `wirelen_extra` lets tests exercise caplen < wirelen.
    fn append_packet(
        buf: &mut Vec<u8>,
        tv_sec: u32,
        tv_usec: u32,
        payload: &[u8],
        wirelen_extra: u32,
    ) {
        let caplen = payload.len() as u32;
        let wirelen = caplen + wirelen_extra;
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(&tv_sec.to_be_bytes());
        buf.extend_from_slice(&tv_usec.to_be_bytes());
        buf.extend_from_slice(&caplen.to_be_bytes());
        buf.extend_from_slice(&wirelen.to_be_bytes());
        buf.extend_from_slice(payload);
    }

    #[test]
    fn zero_packets() {
        let bytes = batch_header(0);
        let (_uuid, pkts) = parse_batch(&bytes).unwrap();
        assert!(pkts.is_empty());
    }

    #[test]
    fn parse_batch_returns_uuid() {
        let uuid_bytes: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        let mut bytes = Vec::with_capacity(BATCH_HDR_LEN);
        bytes.extend_from_slice(&2u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&uuid_bytes);
        let (uuid, pkts) = parse_batch(&bytes).unwrap();
        assert!(pkts.is_empty());
        assert_eq!(uuid, "01020304-0506-0708-090a-0b0c0d0e0f10");
    }

    #[test]
    fn single_packet_roundtrip() {
        let mut bytes = batch_header(1);
        append_packet(&mut bytes, 100, 250_000, &[0xaa, 0xbb, 0xcc, 0xdd], 0);
        let (_uuid, pkts) = parse_batch(&bytes).unwrap();
        assert_eq!(pkts.len(), 1);
        let p = &pkts[0];
        assert_eq!(p.timestamp_us, 100 * 1_000_000 + 250_000);
        assert_eq!(p.caplen, 4);
        assert_eq!(p.wirelen, 4);
        assert_eq!(p.link_type, LINKTYPE_ETHERNET);
        assert_eq!(&p.data[..], &[0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn multiple_packets_preserve_order() {
        let mut bytes = batch_header(3);
        append_packet(&mut bytes, 1, 0, &[0x01], 0);
        append_packet(&mut bytes, 2, 0, &[0x02, 0x02], 0);
        append_packet(&mut bytes, 3, 0, &[0x03, 0x03, 0x03], 0);
        let (_uuid, pkts) = parse_batch(&bytes).unwrap();
        assert_eq!(pkts.len(), 3);
        assert_eq!(&pkts[0].data[..], &[0x01]);
        assert_eq!(&pkts[1].data[..], &[0x02, 0x02]);
        assert_eq!(&pkts[2].data[..], &[0x03, 0x03, 0x03]);
    }

    #[test]
    fn caplen_can_be_less_than_wirelen() {
        let mut bytes = batch_header(1);
        append_packet(&mut bytes, 0, 0, &[0xff; 10], 40);
        let (_uuid, pkts) = parse_batch(&bytes).unwrap();
        assert_eq!(pkts[0].caplen, 10);
        assert_eq!(pkts[0].wirelen, 50);
    }

    #[test]
    fn truncated_batch_header() {
        let bytes = vec![0u8; BATCH_HDR_LEN - 1];
        let err = parse_batch(&bytes).unwrap_err();
        assert!(matches!(err, BatchError::Truncated { .. }));
    }

    #[test]
    fn truncated_pkt_record_header() {
        let mut bytes = batch_header(1);
        // Only 5 of the 18 bytes required for pkt_data_len + pkt_hdr
        bytes.extend_from_slice(&[0u8; 5]);
        let err = parse_batch(&bytes).unwrap_err();
        assert!(matches!(err, BatchError::Truncated { .. }));
    }

    #[test]
    fn tracker_synthesizes_hb_for_cloud_probe_uuid() {
        use crate::heartbeat::{HeartbeatTracker, HEARTBEAT_INTERVAL_US};

        let mut t = HeartbeatTracker::new();
        let mut p = RawPacket {
            timestamp_us: 1_000_000,
            caplen: 14,
            wirelen: 14,
            link_type: LINKTYPE_ETHERNET,
            data: Bytes::from_static(&[0xAA, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08, 0x00]),
            source_id: "u1".to_string(),
        };
        assert!(t.on_packet(&p).is_none());
        p.timestamp_us += HEARTBEAT_INTERVAL_US;
        let hb = t.on_packet(&p).expect("interval reached → HB");
        assert!(hb.is_heartbeat());
        assert_eq!(hb.source_id, "u1");
    }

    #[test]
    fn pkt_data_len_exceeds_buffer() {
        let mut bytes = batch_header(1);
        // Claim 100 bytes of pkt_data but supply only 10
        bytes.extend_from_slice(&100u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes()); // tv_sec
        bytes.extend_from_slice(&0u32.to_be_bytes()); // tv_usec
        bytes.extend_from_slice(&100u32.to_be_bytes()); // caplen
        bytes.extend_from_slice(&100u32.to_be_bytes()); // wirelen
        bytes.extend_from_slice(&[0u8; 10]);
        let err = parse_batch(&bytes).unwrap_err();
        assert!(matches!(err, BatchError::Truncated { .. }));
    }

    mod integration {
        use super::*;

        use std::net::TcpListener;

        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;
        use zeromq::{PushSocket, Socket, SocketSend, ZmqMessage};

        use crate::routing::RoutingSender;
        use ts_common::internal_metrics::MetricsSystem;

        fn test_metrics() -> ts_common::internal_metrics::MetricsWorker {
            let mut sys = MetricsSystem::new();
            sys.register_worker(
                "test",
                &[
                    Metric::CapturePacketsReceived,
                    Metric::CaptureKernelPacketsDropped,
                    Metric::CaptureTruncatedPackets,
                    Metric::CaptureHeartbeatsEmitted,
                    Metric::CaptureBatchesReceived,
                    Metric::CaptureZmqBatchesDropped,
                ],
            )
        }

        /// Reserve a free localhost port by opening and immediately dropping a
        /// TCP listener.
        fn pick_free_port() -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        }

        fn build_sample_batch(packets: &[(u32, u32, &[u8])]) -> Vec<u8> {
            let mut v = batch_header(packets.len() as u16); // uses default zero UUID
            for (tv_sec, tv_usec, payload) in packets {
                append_packet(&mut v, *tv_sec, *tv_usec, payload, 0);
            }
            v
        }

        #[tokio::test]
        async fn integration_receives_packets_from_push_socket() {
            let port = pick_free_port();
            let endpoint = format!("tcp://127.0.0.1:{port}");

            let (tx, mut rx) = mpsc::channel(16);
            let cancel = CancellationToken::new();

            let source = Box::new(CloudProbeSource::new(endpoint.clone(), 100, None));
            let metrics = test_metrics();
            let cancel_clone = cancel.clone();
            let handle = tokio::spawn(async move {
                source
                    .run(RoutingSender::single(tx), metrics, cancel_clone)
                    .await
            });

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let mut pusher = PushSocket::new();
            pusher.connect(&endpoint).await.unwrap();

            let batch =
                build_sample_batch(&[(100, 123_456, &[0xaa, 0xbb][..]), (101, 0, &[0xcc][..])]);
            pusher.send(ZmqMessage::from(batch)).await.unwrap();

            let pkt1 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for first packet")
                .expect("channel closed");
            let pkt2 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for second packet")
                .expect("channel closed");

            assert_eq!(pkt1.timestamp_us, 100 * 1_000_000 + 123_456);
            assert_eq!(&pkt1.data[..], &[0xaa, 0xbb]);
            assert_eq!(pkt2.timestamp_us, 101 * 1_000_000);
            assert_eq!(&pkt2.data[..], &[0xcc]);

            cancel.cancel();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
                .await
                .expect("source task did not exit")
                .expect("join error");
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn integration_truncated_packets_bump_counter() {
            // A remote probe whose snaplen was smaller than the link's max
            // frame produces a record with `caplen < wirelen` — same
            // tail-truncation that bites local lo capture. The counter
            // must surface this so operators can correlate wedged
            // reassembly with a misconfigured probe-side snaplen.
            let port = pick_free_port();
            let endpoint = format!("tcp://127.0.0.1:{port}");

            let (tx, mut rx) = mpsc::channel(16);
            let cancel = CancellationToken::new();

            let source = Box::new(CloudProbeSource::new(endpoint.clone(), 100, None));
            let metrics = test_metrics();
            let metrics_for_assert = metrics.clone();
            let cancel_clone = cancel.clone();
            let handle = tokio::spawn(async move {
                source
                    .run(RoutingSender::single(tx), metrics, cancel_clone)
                    .await
            });

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let mut pusher = PushSocket::new();
            pusher.connect(&endpoint).await.unwrap();

            // First packet: caplen == wirelen (clean), second: caplen <
            // wirelen (truncated by `wirelen_extra=20`).
            let mut batch = batch_header(2);
            append_packet(&mut batch, 1, 0, &[0xaa, 0xbb][..], 0);
            append_packet(&mut batch, 2, 0, &[0xcc][..], 20);
            pusher.send(ZmqMessage::from(batch)).await.unwrap();

            // Drain both packets out so we know the run loop has accounted
            // for them before we read the counter.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for first packet");
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for second packet");

            cancel.cancel();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
                .await
                .expect("source task did not exit")
                .expect("join error");
            assert!(result.is_ok());

            assert_eq!(
                metrics_for_assert
                    .counter(Metric::CaptureTruncatedPackets)
                    .get(),
                1,
                "exactly one truncated packet (the second) should bump the counter"
            );
            assert_eq!(
                metrics_for_assert
                    .counter(Metric::CapturePacketsReceived)
                    .get(),
                2,
                "both packets received should still be counted"
            );
        }

        #[tokio::test]
        async fn integration_malformed_batch_is_dropped() {
            let port = pick_free_port();
            let endpoint = format!("tcp://127.0.0.1:{port}");

            let (tx, mut rx) = mpsc::channel(16);
            let cancel = CancellationToken::new();

            let source = Box::new(CloudProbeSource::new(endpoint.clone(), 100, None));
            let metrics = test_metrics();
            let cancel_clone = cancel.clone();
            let handle = tokio::spawn(async move {
                source
                    .run(RoutingSender::single(tx), metrics, cancel_clone)
                    .await
            });

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let mut pusher = PushSocket::new();
            pusher.connect(&endpoint).await.unwrap();

            pusher.send(ZmqMessage::from(vec![0u8; 10])).await.unwrap();

            let good = build_sample_batch(&[(1, 0, &[0x01][..])]);
            pusher.send(ZmqMessage::from(good)).await.unwrap();

            let pkt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for packet after malformed batch")
                .expect("channel closed");
            assert_eq!(&pkt.data[..], &[0x01]);

            cancel.cancel();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
                .await
                .expect("source task did not exit")
                .expect("join error");
            assert!(result.is_ok());
        }
    }
}
