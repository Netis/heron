//! Thin-probe capture source — the central collector of the distributed eBPF
//! topology.
//!
//! Structurally the mirror of [`crate::cloud_probe`]: where cloud-probe PULLs
//! ZMQ batches, this accepts mTLS connections from remote `heron-probe`
//! instances and reads length-delimited [`wire`] frames off each. A probe does
//! the eBPF capture and frame synthesis on its host; this side decodes the
//! batch, stamps each `RawPacket` with the probe's `source_id`, and hands it to
//! the same [`RoutingSender`] every other source uses — so the downstream
//! pipeline runs byte-for-byte as if the eBPF source were local, process
//! attribution and all.
//!
//! Identity is established by the mTLS handshake: only a probe whose client
//! cert chains to the configured CA connects at all. The `source_id` is the
//! probe's declared batch id when present, else the client cert's CN.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use h_common::config::TlsServerConfig;
use h_common::internal_metrics::{Metric, MetricsWorker};
use h_common::throttle::ThrottledWarn;

use crate::heartbeat::HeartbeatTracker;
use crate::pcap_dump::{PacketDumper, PacketDumperConfig};
use crate::routing::RoutingSender;
use crate::source::CaptureSource;
use crate::wire;

const WARN_THROTTLE: Duration = Duration::from_secs(5);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Central mTLS listener that ingests `RawPacket` batches from remote probes.
pub struct ThinProbeSource {
    listen: String,
    server_config: Arc<ServerConfig>,
    dump_cfg: Option<PacketDumperConfig>,
}

impl ThinProbeSource {
    /// Build from config, loading the mTLS PEM material into a rustls server
    /// config (fails loudly if the cert/key/CA can't be read or don't agree).
    pub fn from_config(
        listen: String,
        tls: &TlsServerConfig,
        dump_cfg: Option<PacketDumperConfig>,
    ) -> crate::Result<Self> {
        let server_config = crate::tls::server_config(&tls.cert, &tls.key, &tls.client_ca)?;
        Ok(Self::new(listen, Arc::new(server_config), dump_cfg))
    }

    /// Lower-level constructor taking a prebuilt rustls server config. Used by
    /// the integration test with an in-memory throwaway PKI.
    pub fn new(
        listen: String,
        server_config: Arc<ServerConfig>,
        dump_cfg: Option<PacketDumperConfig>,
    ) -> Self {
        Self {
            listen,
            server_config,
            dump_cfg,
        }
    }
}

#[async_trait]
impl CaptureSource for ThinProbeSource {
    async fn run(
        self: Box<Self>,
        tx: RoutingSender,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        let acceptor = TlsAcceptor::from(self.server_config.clone());
        let listener = TcpListener::bind(&self.listen).await?;
        tracing::info!("thin-probe: listening on {} (mTLS)", self.listen);

        let mut accept_err_throttle = ThrottledWarn::new(WARN_THROTTLE);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::debug!("thin-probe: cancellation requested, stopping");
                    break;
                }
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, peer)) => {
                            let acceptor = acceptor.clone();
                            let tx = tx.clone();
                            let metrics = metrics.clone();
                            let cancel = cancel.clone();
                            let dump_cfg = self.dump_cfg.clone();
                            // One task per probe connection; flow state is local
                            // to it, matching the per-source contract upstream.
                            tokio::spawn(async move {
                                let peer = peer.to_string();
                                if let Err(e) =
                                    handle_conn(acceptor, stream, &peer, tx, metrics, cancel, dump_cfg)
                                        .await
                                {
                                    tracing::warn!("thin-probe: connection {peer} ended: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            // Accept errors are transient (fd exhaustion, etc.);
                            // keep the listener alive, the cancel token is the
                            // only way out.
                            metrics.counter(Metric::CaptureReadErrors).inc();
                            if let Some(suppressed) = accept_err_throttle.tick() {
                                if suppressed > 0 {
                                    tracing::warn!(suppressed, "thin-probe: accept error (latest of many): {e}");
                                } else {
                                    tracing::warn!("thin-probe: accept error: {e}");
                                }
                            }
                            tokio::time::sleep(ACCEPT_BACKOFF).await;
                        }
                    }
                }
            }
        }

        tracing::info!("thin-probe: stopped {}", self.listen);
        Ok(())
    }
}

/// Handle one probe connection: TLS handshake → resolve identity → read frames
/// → decode → forward packets. Returns when the probe disconnects, the channel
/// closes, or cancellation fires.
async fn handle_conn(
    acceptor: TlsAcceptor,
    stream: tokio::net::TcpStream,
    peer: &str,
    tx: RoutingSender,
    metrics: MetricsWorker,
    cancel: CancellationToken,
    dump_cfg: Option<PacketDumperConfig>,
) -> crate::Result<()> {
    let tls = acceptor.accept(stream).await?;

    // The mTLS handshake already proved the client cert chains to our CA; pull
    // its CN as the identity fallback for batches that don't declare one.
    let peer_cn = {
        let (_io, conn) = tls.get_ref();
        conn.peer_certificates()
            .and_then(|certs| certs.first())
            .and_then(|c| crate::tls::peer_common_name(c.as_ref()))
    };
    tracing::info!(peer, cn = ?peer_cn, "thin-probe: probe connected");

    let mut dumper = dump_cfg.and_then(|c| match PacketDumper::new(c, metrics.clone()) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("thin-probe: packet dump disabled: {e}");
            None
        }
    });

    let mut framed = Framed::new(tls, wire::length_delimited_codec());
    let mut tracker = HeartbeatTracker::new();
    let mut frame_err_throttle = ThrottledWarn::new(WARN_THROTTLE);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            frame = framed.next() => {
                match frame {
                    Some(Ok(bytes)) => {
                        match wire::decode_frame(&bytes) {
                            Ok(batch) => {
                                metrics.counter(Metric::CaptureBatchesReceived).inc();
                                // Probe-declared id wins; else the cert CN; else
                                // the peer address so a packet is never sourceless.
                                let sid = if !batch.source_id.is_empty() {
                                    batch.source_id
                                } else {
                                    peer_cn.clone().unwrap_or_else(|| peer.to_string())
                                };
                                for mut pkt in batch.packets {
                                    pkt.source_id = sid.clone();
                                    let is_hb = pkt.is_heartbeat();
                                    let truncated = pkt.caplen < pkt.wirelen;

                                    // Synthesize a per-source HB if the interval
                                    // elapsed, forwarded before the real packet so
                                    // the sequence stays time-ordered downstream.
                                    if let Some(hb) = tracker.on_packet(&pkt) {
                                        if tx.send(hb).await.is_err() {
                                            if let Some(d) = dumper.as_mut() { d.flush_all(); }
                                            return Ok(());
                                        }
                                        metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                                        if let Some(d) = dumper.as_mut() { d.flush_all(); }
                                    }

                                    if let Some(d) = dumper.as_mut() { d.write(&pkt); }

                                    if tx.send(pkt).await.is_err() {
                                        if let Some(d) = dumper.as_mut() { d.flush_all(); }
                                        return Ok(());
                                    }
                                    if is_hb {
                                        metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                                        if let Some(d) = dumper.as_mut() { d.flush_all(); }
                                    } else {
                                        metrics.counter(Metric::CapturePacketsReceived).inc();
                                        if truncated {
                                            metrics.counter(Metric::CaptureTruncatedPackets).inc();
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                // Drop the whole frame, same coarse policy as
                                // cloud-probe's drop-the-batch. A version skew or
                                // garbage frame from one probe never wedges the rest.
                                metrics.counter(Metric::CaptureZmqBatchesDropped).inc();
                                if let Some(suppressed) = frame_err_throttle.tick() {
                                    if suppressed > 0 {
                                        tracing::warn!(suppressed, peer, "thin-probe: dropping bad frame (latest of many): {err}");
                                    } else {
                                        tracing::warn!(peer, "thin-probe: dropping bad frame: {err}");
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        // Stream-level (framing/IO) error: the connection is
                        // unusable, tear it down. The probe will reconnect.
                        metrics.counter(Metric::CaptureReadErrors).inc();
                        tracing::debug!(peer, "thin-probe: frame read error: {e}");
                        break;
                    }
                    None => {
                        tracing::debug!(peer, "thin-probe: probe disconnected");
                        break;
                    }
                }
            }
        }
    }

    if let Some(d) = dumper.as_mut() {
        d.flush_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use futures_util::SinkExt;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::TlsConnector;

    use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
    use h_common::process::ProcessInfo;

    use crate::packet::RawPacket;
    use crate::testpki::{gen_pki, pick_free_port, write_pem};
    use crate::wire::{self, ProbeBatch};

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

    async fn recv_one(rx: &mut mpsc::Receiver<RawPacket>) -> RawPacket {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timeout waiting for packet")
            .expect("channel closed")
    }

    fn sample_packet(ts: i64, payload: &[u8], process: Option<ProcessInfo>) -> RawPacket {
        RawPacket {
            timestamp_us: ts,
            caplen: payload.len() as u32,
            wirelen: payload.len() as u32,
            link_type: 1,
            data: Bytes::copy_from_slice(payload),
            source_id: "set-by-probe".to_string(),
            process,
        }
    }

    /// End-to-end: a probe completes an mTLS handshake, ships a batch with
    /// process attribution, and the central forwards each `RawPacket` intact —
    /// proving the distributed path is byte-for-byte the local eBPF source's.
    /// Also covers source_id resolution: the probe's declared id when present,
    /// the client-cert CN as fallback.
    #[tokio::test]
    async fn integration_mtls_probe_delivers_packets_with_process() {
        let pki = gen_pki("gateway-1");
        let dir = tempfile::tempdir().unwrap();
        let ca = write_pem(dir.path(), "ca.pem", &pki.ca_pem);
        let server_crt = write_pem(dir.path(), "server.crt", &pki.server_cert_pem);
        let server_key = write_pem(dir.path(), "server.key", &pki.server_key_pem);
        let client_crt = write_pem(dir.path(), "client.crt", &pki.client_cert_pem);
        let client_key = write_pem(dir.path(), "client.key", &pki.client_key_pem);

        let port = pick_free_port();
        let listen = format!("127.0.0.1:{port}");

        let tls_cfg = TlsServerConfig {
            cert: server_crt,
            key: server_key,
            client_ca: ca.clone(),
        };
        let source =
            Box::new(ThinProbeSource::from_config(listen.clone(), &tls_cfg, None).unwrap());

        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let metrics = test_metrics();
        let metrics_assert = metrics.clone();
        let cancel_run = cancel.clone();
        let handle = tokio::spawn(async move {
            source
                .run(RoutingSender::single(tx), metrics, cancel_run)
                .await
        });

        // Give the listener a moment to bind.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // --- probe side: mTLS client ---
        let client_cfg = crate::tls::client_config(&client_crt, &client_key, &ca).unwrap();
        let connector = TlsConnector::from(Arc::new(client_cfg));
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let domain = ServerName::try_from("localhost").unwrap();
        let tls = connector
            .connect(domain, tcp)
            .await
            .expect("mTLS handshake");
        let mut framed = Framed::new(tls, wire::length_delimited_codec());

        // Frame 1: explicit batch source_id + a packet carrying ProcessInfo.
        let proc = ProcessInfo::new(4242, "claude").with_exe(Some("/usr/bin/claude".into()));
        let batch1 = ProbeBatch::new(
            "explicit-id",
            vec![
                sample_packet(1_000_000, &[0xde, 0xad, 0xbe, 0xef], Some(proc)),
                sample_packet(1_000_100, &[0x01, 0x02], None),
            ],
        );
        framed
            .send(wire::encode_frame(&batch1).unwrap())
            .await
            .unwrap();

        // Frame 2: empty batch source_id → central falls back to client-cert CN.
        let batch2 = ProbeBatch::new("", vec![sample_packet(1_000_200, &[0x09], None)]);
        framed
            .send(wire::encode_frame(&batch2).unwrap())
            .await
            .unwrap();

        let p1 = recv_one(&mut rx).await;
        assert_eq!(p1.source_id, "explicit-id");
        assert_eq!(&p1.data[..], &[0xde, 0xad, 0xbe, 0xef]);
        let pi = p1
            .process
            .expect("process attribution preserved over the wire");
        assert_eq!(pi.pid, 4242);
        assert_eq!(pi.comm, "claude");
        assert_eq!(pi.exe.as_deref(), Some("/usr/bin/claude"));

        let p2 = recv_one(&mut rx).await;
        assert_eq!(p2.source_id, "explicit-id");
        assert!(p2.process.is_none());

        let p3 = recv_one(&mut rx).await;
        assert_eq!(
            p3.source_id, pki.client_cn,
            "empty batch source_id must fall back to the client-cert CN"
        );

        assert_eq!(
            metrics_assert.counter(Metric::CaptureBatchesReceived).get(),
            2
        );
        assert_eq!(
            metrics_assert.counter(Metric::CapturePacketsReceived).get(),
            3
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    }

    /// A client whose cert does not chain to the central's configured CA must be
    /// rejected at the handshake — mTLS is the admission boundary, so no frame
    /// from an unauthorized probe ever reaches the pipeline.
    #[tokio::test]
    async fn integration_untrusted_client_cert_is_rejected() {
        let server_pki = gen_pki("gateway-1");
        // A second, unrelated PKI: its client cert chains to a different CA.
        let rogue_pki = gen_pki("rogue");
        let dir = tempfile::tempdir().unwrap();
        let ca = write_pem(dir.path(), "ca.pem", &server_pki.ca_pem);
        let server_crt = write_pem(dir.path(), "server.crt", &server_pki.server_cert_pem);
        let server_key = write_pem(dir.path(), "server.key", &server_pki.server_key_pem);
        let rogue_crt = write_pem(dir.path(), "rogue.crt", &rogue_pki.client_cert_pem);
        let rogue_key = write_pem(dir.path(), "rogue.key", &rogue_pki.client_key_pem);

        let port = pick_free_port();
        let listen = format!("127.0.0.1:{port}");
        let tls_cfg = TlsServerConfig {
            cert: server_crt,
            key: server_key,
            client_ca: ca.clone(), // trusts ONLY server_pki's CA
        };
        let source = Box::new(ThinProbeSource::from_config(listen, &tls_cfg, None).unwrap());

        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cancel_run = cancel.clone();
        let handle = tokio::spawn(async move {
            source
                .run(RoutingSender::single(tx), test_metrics(), cancel_run)
                .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Rogue probe presents a cert signed by the *other* CA.
        let client_cfg = crate::tls::client_config(&rogue_crt, &rogue_key, &ca).unwrap();
        let connector = TlsConnector::from(Arc::new(client_cfg));
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let domain = ServerName::try_from("localhost").unwrap();
        // The handshake (or the first send) fails; either way no packet arrives.
        if let Ok(tls) = connector.connect(domain, tcp).await {
            let mut framed = Framed::new(tls, wire::length_delimited_codec());
            let batch = ProbeBatch::new("rogue", vec![sample_packet(1, &[0xff], None)]);
            let _ = framed.send(wire::encode_frame(&batch).unwrap()).await;
        }

        let got = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            got.is_err() || matches!(got, Ok(None)),
            "an untrusted client cert must not deliver any packet"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    }
}
