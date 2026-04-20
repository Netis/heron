use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use pcap::Capture;
use tokio_util::sync::CancellationToken;
use tracing;

use ts_common::internal_metrics::{Metric, MetricsWorker};

use crate::packet::RawPacket;
use crate::pcap_dump::{PacketDumper, PacketDumperConfig};
use crate::routing::RoutingSender;
use crate::source::CaptureSource;

/// Reads packets from a pcap file on a blocking thread.
pub struct PcapFileSource {
    path: PathBuf,
    stream_id: String,
    dump_cfg: Option<PacketDumperConfig>,
}

impl PcapFileSource {
    pub fn new(path: PathBuf, stream_id: String, dump_cfg: Option<PacketDumperConfig>) -> Self {
        Self {
            path,
            stream_id,
            dump_cfg,
        }
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
        let stream_id = self.stream_id.clone();
        let dump_cfg = self.dump_cfg.clone();
        let dumper_metrics = metrics.clone();

        // Run pcap reading on a blocking thread since next_packet() is blocking.
        let result = tokio::task::spawn_blocking(move || -> crate::Result<()> {
            let mut dumper = dump_cfg.and_then(|c| match PacketDumper::new(c, dumper_metrics) {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!("pcap-file: packet dump disabled: {e}");
                    None
                }
            });
            let mut cap = Capture::from_file(&path)?;
            let link_type = cap.get_datalink().0 as u32;

            tracing::info!(
                "pcap-file: opened {} (link_type={})",
                path.display(),
                link_type
            );

            let mut count: u64 = 0;
            loop {
                if cancel.is_cancelled() {
                    tracing::debug!("pcap-file: cancellation requested, stopping");
                    break;
                }

                match cap.next_packet() {
                    Ok(packet) => {
                        let ts = packet.header.ts;
                        let timestamp_us = ts.tv_sec as i64 * 1_000_000 + ts.tv_usec as i64;

                        let raw = RawPacket {
                            timestamp_us,
                            caplen: packet.header.caplen,
                            wirelen: packet.header.len,
                            link_type,
                            data: Bytes::copy_from_slice(packet.data),
                            stream_id: stream_id.clone(),
                        };

                        if let Some(d) = dumper.as_mut() {
                            d.write(&raw);
                        }

                        // If the receiver is gone, stop reading.
                        if tx.blocking_send(raw).is_err() {
                            tracing::debug!("pcap-file: channel closed, stopping");
                            break;
                        }

                        count += 1;
                        metrics.counter(Metric::CapturePacketsReceived).add(1);
                    }
                    Err(pcap::Error::NoMorePackets) => {
                        tracing::debug!("pcap-file: end of file reached");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("pcap-file: read error: {e}");
                        return Err(e.into());
                    }
                }
            }

            if let Some(d) = dumper.as_mut() {
                d.flush_all();
            }
            tracing::info!("pcap-file: finished reading {} packets", count);
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
    use ts_common::internal_metrics::{Metric, MetricsSystem};

    /// Create a MetricsWorker suitable for capture tests.
    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        sys.register_worker(
            "test",
            &[
                Metric::CapturePacketsReceived,
                Metric::CapturePacketsDropped,
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
}
