//! Transport stress tests for the distributed-capture pair (`ProbeUplink` ↔
//! `ThinProbeSource`) — pure transport, no pipeline, no eBPF, no root. Run under
//! plain `cargo test`. Covers: many probes → one central (zero loss + per-source
//! routing), probe churn/restart, backpressure (bounded + lossless), and a
//! version-skew/garbage frame not wedging the connection or its neighbours.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use bytes::Bytes;
use futures_util::SinkExt;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use h_common::config::{TlsClientConfig, TlsServerConfig};
use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};

use crate::packet::RawPacket;
use crate::routing::RoutingSender;
use crate::source::CaptureSource;
use crate::testpki::{gen_pki, pick_free_port, write_pem};
use crate::thin_probe::ThinProbeSource;
use crate::wire::{self, ProbeBatch};
use crate::ProbeUplink;

// ---- shared scaffolding ---------------------------------------------------

/// A throwaway PKI written to a tempdir; all probes reuse the one client cert
/// (mTLS identity is "chains to the CA"; per-probe identity rides in the batch
/// `source_id`). The tempdir is kept alive for the PEM files' lifetime.
struct Pki {
    _dir: TempDir,
    ca: String,
    server_crt: String,
    server_key: String,
    client_crt: String,
    client_key: String,
}

fn pki() -> Pki {
    let dir = tempfile::tempdir().unwrap();
    let p = gen_pki("probe");
    Pki {
        ca: write_pem(dir.path(), "ca.pem", &p.ca_pem),
        server_crt: write_pem(dir.path(), "server.crt", &p.server_cert_pem),
        server_key: write_pem(dir.path(), "server.key", &p.server_key_pem),
        client_crt: write_pem(dir.path(), "client.crt", &p.client_cert_pem),
        client_key: write_pem(dir.path(), "client.key", &p.client_key_pem),
        _dir: dir,
    }
}

impl Pki {
    fn server_tls(&self) -> TlsServerConfig {
        TlsServerConfig {
            cert: self.server_crt.clone(),
            key: self.server_key.clone(),
            client_ca: self.ca.clone(),
        }
    }
    fn client_tls(&self) -> TlsClientConfig {
        TlsClientConfig {
            cert: self.client_crt.clone(),
            key: self.client_key.clone(),
            server_ca: self.ca.clone(),
        }
    }
}

fn test_metrics() -> MetricsWorker {
    MetricsSystem::new().register_worker(
        "thin-probe",
        &[
            Metric::CaptureBatchesReceived,
            Metric::CapturePacketsReceived,
            Metric::CaptureTruncatedPackets,
            Metric::CaptureHeartbeatsEmitted,
            Metric::CaptureZmqBatchesDropped,
            Metric::CaptureReadErrors,
        ],
    )
}

/// A non-heartbeat packet with a payload tag; constant timestamp so the central
/// synthesizes no heartbeats.
fn pkt(tag: &str) -> RawPacket {
    RawPacket {
        timestamp_us: 1_000_000,
        caplen: tag.len() as u32,
        wirelen: tag.len() as u32,
        link_type: 1,
        data: Bytes::copy_from_slice(tag.as_bytes()),
        source_id: "set-by-central".to_string(),
        process: None,
    }
}

/// Re-derivation of `RoutingSender`'s shard index (its documented contract:
/// `hash(source_id) % n` with the std `DefaultHasher`).
fn expected_shard(source_id: &str, n: usize) -> usize {
    let mut h = DefaultHasher::new();
    source_id.hash(&mut h);
    (h.finish() as usize) % n
}

fn spawn_central(
    listen: &str,
    tls: &TlsServerConfig,
    router: RoutingSender,
    metrics: MetricsWorker,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let central = ThinProbeSource::from_config(listen.to_string(), tls, None).unwrap();
    tokio::spawn(async move {
        let _ = Box::new(central).run(router, metrics, cancel).await;
    })
}

/// Poll a counter until it stops climbing (the central has drained the socket),
/// bounded by ~10s.
async fn wait_quiescent(metrics: &MetricsWorker, metric: Metric) -> u64 {
    let mut prev = u64::MAX;
    for _ in 0..200 {
        let cur = metrics.counter(metric).get();
        if cur == prev {
            return cur;
        }
        prev = cur;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    prev
}

// ---- 3.1: many probes → one central ---------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_probes_zero_loss_and_source_id_routing() {
    const N: usize = 50;
    const K: usize = 20;
    const SHARDS: usize = 4;

    let pki = pki();
    let port = pick_free_port();
    let listen = format!("127.0.0.1:{port}");

    // Central with a real multi-shard router (single-shard routing is a no-op).
    let mut shard_rxs = Vec::with_capacity(SHARDS);
    let mut shard_txs = Vec::with_capacity(SHARDS);
    for _ in 0..SHARDS {
        let (tx, rx) = mpsc::channel::<RawPacket>(N * K + 16);
        shard_txs.push(tx);
        shard_rxs.push(rx);
    }
    let metrics = test_metrics();
    let metrics_probe = metrics.clone();
    let cancel = CancellationToken::new();
    let central = spawn_central(
        &listen,
        &pki.server_tls(),
        RoutingSender::new(shard_txs),
        metrics,
        cancel.clone(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 50 probes, distinct source_id, each ships K packets then disconnects.
    let mut uplink_handles = Vec::with_capacity(N);
    for i in 0..N {
        let sid = format!("probe-{i}");
        let uplink = ProbeUplink::from_config(
            format!("127.0.0.1:{port}"),
            "localhost".to_string(),
            sid.clone(),
            &pki.client_tls(),
        )
        .unwrap()
        .with_batching(8, Duration::from_millis(10));
        let (tx, rx) = mpsc::channel::<RawPacket>(K + 1);
        let cancel_u = cancel.clone();
        let h = tokio::spawn(async move { uplink.run(rx, cancel_u).await });
        for k in 0..K {
            tx.send(pkt(&format!("{sid}-{k}"))).await.unwrap();
        }
        drop(tx); // EOF → uplink ships + closes
        uplink_handles.push(h);
    }
    for h in uplink_handles {
        let _ = timeout(Duration::from_secs(30), h)
            .await
            .expect("uplink done");
    }

    // Wait for the central to receive + forward every packet, then tear down.
    let received = wait_quiescent(&metrics_probe, Metric::CapturePacketsReceived).await;
    assert_eq!(
        received,
        (N * K) as u64,
        "every packet must reach the central"
    );
    cancel.cancel();
    let _ = timeout(Duration::from_secs(10), central).await;

    // Drain the shards: count per source + assert each source landed on exactly
    // its hashed shard (isolation — no source split across shards).
    let mut total = 0usize;
    let mut per_source: HashMap<String, usize> = HashMap::new();
    for (shard_idx, mut rx) in shard_rxs.into_iter().enumerate() {
        while let Ok(p) = rx.try_recv() {
            total += 1;
            assert_eq!(
                shard_idx,
                expected_shard(&p.source_id, SHARDS),
                "{} routed to the wrong shard",
                p.source_id
            );
            *per_source.entry(p.source_id).or_default() += 1;
        }
    }
    assert_eq!(total, N * K, "zero loss across all shards");
    assert_eq!(per_source.len(), N, "all {N} probes present");
    for i in 0..N {
        assert_eq!(per_source[&format!("probe-{i}")], K, "probe-{i} count");
    }
}

// ---- 3.2: churn / probe restart -------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_restart_delivers_both_bursts() {
    const K: usize = 16;
    let pki = pki();
    let port = pick_free_port();
    let listen = format!("127.0.0.1:{port}");

    let (tx, mut rx) = mpsc::channel::<RawPacket>(4 * K);
    let metrics = test_metrics();
    let metrics_probe = metrics.clone();
    let cancel = CancellationToken::new();
    let central = spawn_central(
        &listen,
        &pki.server_tls(),
        RoutingSender::single(tx),
        metrics,
        cancel.clone(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Two sequential uplinks with the SAME source_id (a probe that restarts).
    for burst in 0..2 {
        let uplink = ProbeUplink::from_config(
            format!("127.0.0.1:{port}"),
            "localhost".to_string(),
            "churn-1".to_string(),
            &pki.client_tls(),
        )
        .unwrap()
        .with_batching(8, Duration::from_millis(10));
        let (utx, urx) = mpsc::channel::<RawPacket>(K + 1);
        let cancel_u = cancel.clone();
        let h = tokio::spawn(async move { uplink.run(urx, cancel_u).await });
        for k in 0..K {
            utx.send(pkt(&format!("burst{burst}-{k}"))).await.unwrap();
        }
        drop(utx);
        let _ = timeout(Duration::from_secs(20), h)
            .await
            .expect("burst shipped");
    }

    let received = wait_quiescent(&metrics_probe, Metric::CapturePacketsReceived).await;
    assert_eq!(
        received,
        (2 * K) as u64,
        "both bursts delivered across the restart"
    );
    cancel.cancel();
    let _ = timeout(Duration::from_secs(10), central).await;

    let mut got = 0usize;
    while let Ok(p) = rx.try_recv() {
        assert_eq!(p.source_id, "churn-1", "source_id stable across reconnect");
        got += 1;
    }
    assert_eq!(got, 2 * K);
}

// ---- 3.3: backpressure stays bounded + lossless ---------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_is_bounded_and_lossless() {
    const N: usize = 500;
    let pki = pki();
    let port = pick_free_port();
    let listen = format!("127.0.0.1:{port}");

    // Deliberately TINY central channel; a slow consumer drains it.
    let (tx, mut rx) = mpsc::channel::<RawPacket>(2);
    let metrics = test_metrics();
    let cancel = CancellationToken::new();
    let central = spawn_central(
        &listen,
        &pki.server_tls(),
        RoutingSender::single(tx),
        metrics,
        cancel.clone(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    let uplink = ProbeUplink::from_config(
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        "bp".to_string(),
        &pki.client_tls(),
    )
    .unwrap()
    .with_batching(16, Duration::from_millis(10));
    let (utx, urx) = mpsc::channel::<RawPacket>(8);
    let cancel_u = cancel.clone();
    let uplink_handle = tokio::spawn(async move { uplink.run(urx, cancel_u).await });

    // Producer pushes fast; the bounded utx makes it block when the chain is
    // backed up — no unbounded growth.
    let producer = tokio::spawn(async move {
        for k in 0..N {
            if utx.send(pkt(&format!("bp-{k}"))).await.is_err() {
                break;
            }
        }
        drop(utx);
    });

    // Slow consumer: every packet is received (lossless), just slowly. If the
    // chain grew unbounded or deadlocked, this loop would never reach N and the
    // outer timeout would fire.
    let consume = async {
        let mut got = 0usize;
        while got < N {
            match rx.recv().await {
                Some(_) => {
                    got += 1;
                    if got % 50 == 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
                None => break,
            }
        }
        got
    };
    let got = timeout(Duration::from_secs(30), consume)
        .await
        .expect("backpressure must not deadlock");
    assert_eq!(got, N, "every packet arrives under backpressure (lossless)");

    let _ = timeout(Duration::from_secs(10), producer).await;
    cancel.cancel();
    let _ = timeout(Duration::from_secs(10), uplink_handle).await;
    let _ = timeout(Duration::from_secs(10), central).await;
}

// ---- 3.4: version-skew / garbage frame does not wedge ---------------------

/// A raw mTLS client that sends pre-built frames (used to inject a bad-version /
/// garbage frame the well-behaved ProbeUplink never would).
async fn raw_frames(port: u16, tls: &TlsClientConfig, frames: Vec<Bytes>) {
    let connector = TlsConnector::from(std::sync::Arc::new(
        crate::tls::client_config(&tls.cert, &tls.key, &tls.server_ca).unwrap(),
    ));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let domain = ServerName::try_from("localhost").unwrap();
    let stream = connector.connect(domain, tcp).await.expect("handshake");
    let mut framed = Framed::new(stream, wire::length_delimited_codec());
    for f in frames {
        framed.send(f).await.unwrap();
    }
    let _ = framed.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_frame_does_not_wedge_connection_or_neighbours() {
    let pki = pki();
    let port = pick_free_port();
    let listen = format!("127.0.0.1:{port}");

    let (tx, mut rx) = mpsc::channel::<RawPacket>(64);
    let metrics = test_metrics();
    let metrics_probe = metrics.clone();
    let cancel = CancellationToken::new();
    let central = spawn_central(
        &listen,
        &pki.server_tls(),
        RoutingSender::single(tx),
        metrics,
        cancel.clone(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Frame 1 good, frame 2 bad version, frame 3 garbage body, frame 4 good — on
    // ONE connection. The good frames must survive the bad ones in between.
    let good1 = wire::encode_frame(&ProbeBatch::new("skew", vec![pkt("good-1")])).unwrap();
    let mut bad_version = wire::encode_frame(&ProbeBatch::new("skew", vec![pkt("nope")]))
        .unwrap()
        .to_vec();
    bad_version[0] = wire::PROTOCOL_VERSION.wrapping_add(9);
    let garbage = Bytes::from_static(&[wire::PROTOCOL_VERSION, 0xFF, 0xFF, 0xFF, 0xFF]);
    let good2 = wire::encode_frame(&ProbeBatch::new("skew", vec![pkt("good-2")])).unwrap();
    raw_frames(
        port,
        &pki.client_tls(),
        vec![good1, Bytes::from(bad_version), garbage, good2],
    )
    .await;

    // A concurrent, well-behaved probe must be unaffected by the other's bad frames.
    let uplink = ProbeUplink::from_config(
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        "neighbour".to_string(),
        &pki.client_tls(),
    )
    .unwrap()
    .with_batching(1, Duration::from_millis(10));
    let (utx, urx) = mpsc::channel::<RawPacket>(4);
    let cancel_u = cancel.clone();
    let nb = tokio::spawn(async move { uplink.run(urx, cancel_u).await });
    utx.send(pkt("nb-1")).await.unwrap();
    drop(utx);
    let _ = timeout(Duration::from_secs(20), nb).await;

    // 3 good packets total (good-1, good-2, nb-1); 2 bad frames dropped+counted.
    let received = wait_quiescent(&metrics_probe, Metric::CapturePacketsReceived).await;
    assert_eq!(
        received, 3,
        "good packets survive the bad frames + the neighbour"
    );
    cancel.cancel();
    let _ = timeout(Duration::from_secs(10), central).await;

    let mut tags: Vec<String> = Vec::new();
    while let Ok(p) = rx.try_recv() {
        tags.push(String::from_utf8_lossy(&p.data).to_string());
    }
    tags.sort();
    assert_eq!(tags, vec!["good-1", "good-2", "nb-1"]);
    assert_eq!(
        metrics_probe
            .counter(Metric::CaptureZmqBatchesDropped)
            .get(),
        2,
        "the bad-version + garbage frames are each counted as a dropped batch"
    );
}
