//! Probe-side uplink — the TLS-client counterpart of [`crate::thin_probe`].
//!
//! On an edge host, `heron-probe` runs the existing [`EbpfSource`](crate::ebpf)
//! into a local channel; this uplink drains that channel, bundles packets into
//! [`ProbeBatch`](crate::wire::ProbeBatch)es, and ships them to the central over
//! an mTLS stream framed by [`wire::length_delimited_codec`]. The eBPF capture +
//! frame synthesis is unchanged and platform-gated; this transport is plain
//! cross-platform Tokio, so it builds and is tested on every host.
//!
//! Resilience:
//! - **Reconnect** — the probe dials out (NAT/firewall friendly) and retries
//!   with capped exponential backoff, so a central restart or network blip
//!   self-heals without operator action.
//! - **Backpressure** — capture feeds the uplink through a bounded channel. When
//!   connected but slow, TCP backpressure flows back through the channel to the
//!   eBPF perf buffer, which drops by absolute seq offset (graceful, never a
//!   torn body — see `synth.rs`). The bounded queue is the OOM guard.

use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::TlsConnector;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;

use h_common::config::TlsClientConfig;

use crate::packet::RawPacket;
use crate::wire::{self, ProbeBatch};
use crate::{CaptureError, Result};

/// First reconnect delay; doubles per failed attempt up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Defaults when the binary doesn't override them.
const DEFAULT_BATCH_MAX_PACKETS: usize = 256;
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(100);

type ClientFramed = Framed<TlsStream<TcpStream>, LengthDelimitedCodec>;

/// Drives the probe→central uplink for the lifetime of the probe.
pub struct ProbeUplink {
    central_endpoint: String,
    server_name: String,
    source_id: String,
    client_config: Arc<ClientConfig>,
    batch_max_packets: usize,
    flush_interval: Duration,
}

enum PumpExit {
    /// Cancellation token fired.
    Cancelled,
    /// The capture channel closed (eBPF source ended).
    SourceClosed,
    /// The stream broke; reconnect.
    Disconnected,
}

impl ProbeUplink {
    /// Build from config, loading the probe's client cert/key and the central's
    /// CA from PEM. `server_name` is the SNI / cert name to validate the central
    /// against (must match a SAN in the central's server cert).
    pub fn from_config(
        central_endpoint: String,
        server_name: String,
        source_id: String,
        tls: &TlsClientConfig,
    ) -> Result<Self> {
        let client_config = crate::tls::client_config(&tls.cert, &tls.key, &tls.server_ca)?;
        Ok(Self::new(
            central_endpoint,
            server_name,
            source_id,
            Arc::new(client_config),
        ))
    }

    /// Lower-level constructor taking a prebuilt rustls client config. Used by
    /// the loopback test with an in-memory throwaway PKI.
    pub fn new(
        central_endpoint: String,
        server_name: String,
        source_id: String,
        client_config: Arc<ClientConfig>,
    ) -> Self {
        Self {
            central_endpoint,
            server_name,
            source_id,
            client_config,
            batch_max_packets: DEFAULT_BATCH_MAX_PACKETS,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
        }
    }

    /// Override batching knobs (packets per frame, max time a packet waits).
    pub fn with_batching(mut self, batch_max_packets: usize, flush_interval: Duration) -> Self {
        self.batch_max_packets = batch_max_packets.max(1);
        self.flush_interval = flush_interval;
        self
    }

    /// Run until cancelled or the capture channel closes. Reconnects on its own.
    pub async fn run(
        self,
        mut rx: mpsc::Receiver<RawPacket>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if cancel.is_cancelled() {
                break;
            }
            match self.connect().await {
                Ok(framed) => {
                    tracing::info!(
                        central = %self.central_endpoint,
                        source_id = %self.source_id,
                        "heron-probe: uplink connected",
                    );
                    backoff = INITIAL_BACKOFF;
                    match self.pump(framed, &mut rx, &cancel).await {
                        PumpExit::Cancelled | PumpExit::SourceClosed => break,
                        PumpExit::Disconnected => {
                            tracing::warn!("heron-probe: uplink disconnected, reconnecting");
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        central = %self.central_endpoint,
                        "heron-probe: connect failed: {e}; retry in {backoff:?}",
                    );
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        tracing::info!("heron-probe: uplink stopped");
        Ok(())
    }

    async fn connect(&self) -> Result<ClientFramed> {
        let tcp = TcpStream::connect(&self.central_endpoint).await?;
        tcp.set_nodelay(true).ok();
        let connector = TlsConnector::from(self.client_config.clone());
        let domain = ServerName::try_from(self.server_name.clone())
            .map_err(|e| CaptureError::Tls(format!("invalid server name: {e}")))?;
        let tls = connector.connect(domain, tcp).await?;
        Ok(Framed::new(tls, wire::length_delimited_codec()))
    }

    /// Drain the capture channel into batched frames until disconnect/cancel/EOF.
    async fn pump(
        &self,
        mut framed: ClientFramed,
        rx: &mut mpsc::Receiver<RawPacket>,
        cancel: &CancellationToken,
    ) -> PumpExit {
        let mut batch: Vec<RawPacket> = Vec::with_capacity(self.batch_max_packets);
        let mut ticker = tokio::time::interval(self.flush_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = self.flush(&mut framed, &mut batch).await;
                    self.graceful_close(&mut framed).await;
                    return PumpExit::Cancelled;
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(pkt) => {
                            batch.push(pkt);
                            if batch.len() >= self.batch_max_packets
                                && self.flush(&mut framed, &mut batch).await.is_err()
                            {
                                return PumpExit::Disconnected;
                            }
                        }
                        None => {
                            let _ = self.flush(&mut framed, &mut batch).await;
                            self.graceful_close(&mut framed).await;
                            return PumpExit::SourceClosed;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if !batch.is_empty() && self.flush(&mut framed, &mut batch).await.is_err() {
                        return PumpExit::Disconnected;
                    }
                }
            }
        }
    }

    /// Cleanly shut the stream down on a *graceful* exit (capture ended /
    /// cancelled) so the central sees a TLS `close_notify` + clean EOF rather
    /// than a dirty close it would count as a read error. Best-effort: on a
    /// broken stream this is a no-op.
    async fn graceful_close(&self, framed: &mut ClientFramed) {
        let _ = framed.close().await;
    }

    /// Encode and ship the accumulated batch. `Err(())` means the stream broke
    /// (reconnect); an *encode* failure drops the batch but keeps the stream.
    async fn flush(
        &self,
        framed: &mut ClientFramed,
        batch: &mut Vec<RawPacket>,
    ) -> std::result::Result<(), ()> {
        if batch.is_empty() {
            return Ok(());
        }
        let probe_batch = ProbeBatch::new(self.source_id.clone(), std::mem::take(batch));
        let frame = match wire::encode_frame(&probe_batch) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("heron-probe: dropping unencodable batch: {e}");
                return Ok(());
            }
        };
        match framed.send(frame).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!("heron-probe: frame send failed: {e}");
                Err(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;

    use h_common::config::TlsServerConfig;
    use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
    use h_common::process::ProcessInfo;

    use crate::routing::RoutingSender;
    use crate::source::CaptureSource;
    use crate::testpki::{gen_pki, pick_free_port, write_pem};
    use crate::thin_probe::ThinProbeSource;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        sys.register_worker(
            "test",
            &[
                Metric::CapturePacketsReceived,
                Metric::CaptureTruncatedPackets,
                Metric::CaptureHeartbeatsEmitted,
                Metric::CaptureBatchesReceived,
                Metric::CaptureZmqBatchesDropped,
                Metric::CaptureReadErrors,
            ],
        )
    }

    fn raw(ts: i64, payload: &[u8], process: Option<ProcessInfo>) -> RawPacket {
        RawPacket {
            timestamp_us: ts,
            caplen: payload.len() as u32,
            wirelen: payload.len() as u32,
            link_type: 1,
            data: Bytes::copy_from_slice(payload),
            source_id: String::new(),
            process,
        }
    }

    async fn recv_one(rx: &mut mpsc::Receiver<RawPacket>) -> RawPacket {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timeout waiting for packet at central")
            .expect("central channel closed")
    }

    /// Spin up a central ThinProbeSource and a ProbeUplink over loopback mTLS,
    /// feed RawPackets into the uplink, and assert they arrive at the central's
    /// RoutingSender — process attribution and bytes intact. This is the full
    /// Phase-1 transport round trip with no eBPF involved.
    #[tokio::test]
    async fn uplink_delivers_packets_to_central() {
        let pki = gen_pki("probe-x");
        let dir = tempfile::tempdir().unwrap();
        let ca = write_pem(dir.path(), "ca.pem", &pki.ca_pem);
        let server_crt = write_pem(dir.path(), "server.crt", &pki.server_cert_pem);
        let server_key = write_pem(dir.path(), "server.key", &pki.server_key_pem);
        let client_crt = write_pem(dir.path(), "client.crt", &pki.client_cert_pem);
        let client_key = write_pem(dir.path(), "client.key", &pki.client_key_pem);

        let port = pick_free_port();
        let listen = format!("127.0.0.1:{port}");
        let cancel = CancellationToken::new();

        // --- central ---
        let server_tls = TlsServerConfig {
            cert: server_crt,
            key: server_key,
            client_ca: ca.clone(),
        };
        let central = Box::new(ThinProbeSource::from_config(listen, &server_tls, None).unwrap());
        let (ctx, mut crx) = mpsc::channel(64);
        let cancel_c = cancel.clone();
        let central_handle = tokio::spawn(async move {
            central
                .run(RoutingSender::single(ctx), test_metrics(), cancel_c)
                .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        // --- probe uplink ---
        let client_tls = TlsClientConfig {
            cert: client_crt,
            key: client_key,
            server_ca: ca,
        };
        let uplink = ProbeUplink::from_config(
            format!("127.0.0.1:{port}"),
            "localhost".to_string(),
            "probe-x".to_string(),
            &client_tls,
        )
        .unwrap()
        .with_batching(2, Duration::from_millis(40));
        let (ptx, prx) = mpsc::channel(64);
        let cancel_p = cancel.clone();
        let uplink_handle = tokio::spawn(async move { uplink.run(prx, cancel_p).await });

        // Feed three packets; one carries process attribution.
        let proc = ProcessInfo::new(99, "node").with_exe(Some("/usr/bin/node".into()));
        ptx.send(raw(1_000_000, &[0xaa, 0xbb], Some(proc)))
            .await
            .unwrap();
        ptx.send(raw(1_000_010, &[0xcc], None)).await.unwrap();
        ptx.send(raw(1_000_020, &[0xdd, 0xee, 0xff], None))
            .await
            .unwrap();

        let p1 = recv_one(&mut crx).await;
        assert_eq!(p1.source_id, "probe-x");
        assert_eq!(&p1.data[..], &[0xaa, 0xbb]);
        let pi = p1.process.expect("process attribution survives the uplink");
        assert_eq!(pi.pid, 99);
        assert_eq!(pi.comm, "node");
        assert_eq!(pi.exe.as_deref(), Some("/usr/bin/node"));

        let p2 = recv_one(&mut crx).await;
        assert_eq!(&p2.data[..], &[0xcc]);
        let p3 = recv_one(&mut crx).await;
        assert_eq!(&p3.data[..], &[0xdd, 0xee, 0xff]);
        assert_eq!(p3.source_id, "probe-x");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), central_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), uplink_handle).await;
    }

    /// The uplink starts before the central is listening: its first connect
    /// fails, it backs off and retries, and once the central comes up the
    /// buffered packets flow through. Exercises the reconnect path.
    #[tokio::test]
    async fn uplink_reconnects_when_central_starts_late() {
        let pki = gen_pki("probe-late");
        let dir = tempfile::tempdir().unwrap();
        let ca = write_pem(dir.path(), "ca.pem", &pki.ca_pem);
        let server_crt = write_pem(dir.path(), "server.crt", &pki.server_cert_pem);
        let server_key = write_pem(dir.path(), "server.key", &pki.server_key_pem);
        let client_crt = write_pem(dir.path(), "client.crt", &pki.client_cert_pem);
        let client_key = write_pem(dir.path(), "client.key", &pki.client_key_pem);

        let port = pick_free_port();
        let cancel = CancellationToken::new();

        // Start the uplink first — nothing is listening yet.
        let client_tls = TlsClientConfig {
            cert: client_crt,
            key: client_key,
            server_ca: ca.clone(),
        };
        let uplink = ProbeUplink::from_config(
            format!("127.0.0.1:{port}"),
            "localhost".to_string(),
            "probe-late".to_string(),
            &client_tls,
        )
        .unwrap()
        .with_batching(1, Duration::from_millis(40));
        let (ptx, prx) = mpsc::channel(64);
        let cancel_p = cancel.clone();
        let uplink_handle = tokio::spawn(async move { uplink.run(prx, cancel_p).await });

        // Queue a packet while disconnected; it waits in the bounded channel.
        ptx.send(raw(1_000_000, &[0x42], None)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await; // at least one failed connect

        // Now bring the central up.
        let server_tls = TlsServerConfig {
            cert: server_crt,
            key: server_key,
            client_ca: ca,
        };
        let central = Box::new(
            ThinProbeSource::from_config(format!("127.0.0.1:{port}"), &server_tls, None).unwrap(),
        );
        let (ctx, mut crx) = mpsc::channel(64);
        let cancel_c = cancel.clone();
        let central_handle = tokio::spawn(async move {
            central
                .run(RoutingSender::single(ctx), test_metrics(), cancel_c)
                .await
        });

        // The queued packet (and a fresh one) should arrive after reconnect.
        ptx.send(raw(1_000_050, &[0x43], None)).await.unwrap();
        let first = recv_one(&mut crx).await;
        assert_eq!(first.source_id, "probe-late");
        assert_eq!(&first.data[..], &[0x42]);

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), central_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), uplink_handle).await;
    }
}
