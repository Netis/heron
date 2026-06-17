//! Wire-equivalence differential — the correctness keystone for the distributed
//! eBPF capture topology.
//!
//! The design's central claim is that the central collector runs the *exact same*
//! downstream pipeline as a local eBPF source — the split is at `RawPacket`, so
//! the bytes are identical. This test proves it: replay each corpus fixture two
//! ways and assert the extracted turns/calls are identical.
//!
//!   (a) LOCAL:        pcap → pipeline                    (today's path)
//!   (b) DISTRIBUTED:  pcap → ProbeUplink ──mTLS──▶ ThinProbeSource → pipeline
//!
//! Both feed the *same* pipeline graph (`common::build_pipeline`) via the same
//! `PcapFileSource`, so any difference is attributable to the wire alone. The
//! projection is `source_id`-free (the central restamps `source_id`, which can't
//! affect turns/calls — flow grouping keys on the 5-tuple inside the bytes), so
//! the assertion needs no normalization. There is no golden here — the existing
//! corpus goldens are the transitive ground truth; this is a pure differential.
//!
//! LFS/absent fixtures are skipped exactly as in `corpus_golden.rs`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use h_capture::testpki::{gen_pki, pick_free_port, write_pem};
use h_capture::{
    CaptureSource, PcapFileSource, ProbeUplink, RawPacket, RoutingSender, ThinProbeSource,
};
use h_common::config::{TlsClientConfig, TlsServerConfig};
use h_common::internal_metrics::{Metric, MetricsSystem};

use common::PipelineOutput;

/// The Capture* counters `ThinProbeSource` writes (must all be registered or
/// `counter()` panics).
const THIN_PROBE_METRICS: &[Metric] = &[
    Metric::CaptureBatchesReceived,
    Metric::CapturePacketsReceived,
    Metric::CaptureTruncatedPackets,
    Metric::CaptureHeartbeatsEmitted,
    Metric::CaptureZmqBatchesDropped,
    Metric::CaptureReadErrors,
];

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// Replay `file` through the full distributed transport into the same pipeline a
/// local run uses, and collect the central-side turns/calls.
async fn run_distributed(file: &str) -> Option<PipelineOutput> {
    let path = common::fixture(file)?;

    // Throwaway PKI (one CA → server cert SAN localhost + client cert).
    let dir = tempfile::tempdir().unwrap();
    let pki = gen_pki("probe-diff");
    let ca = write_pem(dir.path(), "ca.pem", &pki.ca_pem);
    let server_crt = write_pem(dir.path(), "server.crt", &pki.server_cert_pem);
    let server_key = write_pem(dir.path(), "server.key", &pki.server_key_pem);
    let client_crt = write_pem(dir.path(), "client.crt", &pki.client_cert_pem);
    let client_key = write_pem(dir.path(), "client.key", &pki.client_key_pem);
    let port = pick_free_port();
    let listen = format!("127.0.0.1:{port}");

    // The central's RoutingSender IS the pipeline's capture-ingress — downstream
    // of `raw_tx`, nothing knows a ThinProbeSource (not a PcapFileSource) fed it.
    let (raw_tx, drain) = common::build_pipeline();

    let cancel = CancellationToken::new();

    // Central: ThinProbeSource → pipeline.
    let mut central_metrics_sys = MetricsSystem::new();
    let central_metrics = central_metrics_sys.register_worker("thin-probe", THIN_PROBE_METRICS);
    // A clone for the drain-quiescence poll below (the worker handle is moved into
    // the central task).
    let central_metrics_probe = central_metrics.clone();
    let _central_svc = central_metrics_sys.start();
    let server_tls = TlsServerConfig {
        cert: server_crt,
        key: server_key,
        client_ca: ca.clone(),
    };
    let central = ThinProbeSource::from_config(listen, &server_tls, None).unwrap();
    let central_cancel = cancel.clone();
    let central_handle = tokio::spawn(async move {
        let _ = Box::new(central)
            .run(
                RoutingSender::single(raw_tx),
                central_metrics,
                central_cancel,
            )
            .await;
    });
    // Let the listener bind before the probe dials (the uplink would retry, but
    // this avoids reconnect churn).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Probe uplink (edge → central).
    let client_tls = TlsClientConfig {
        cert: client_crt,
        key: client_key,
        server_ca: ca,
    };
    let uplink = ProbeUplink::from_config(
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        "probe-diff".to_string(),
        &client_tls,
    )
    .unwrap()
    .with_batching(64, Duration::from_millis(10));
    let (uplink_tx, uplink_rx) = mpsc::channel::<RawPacket>(4096);
    let uplink_cancel = cancel.clone();
    let uplink_handle = tokio::spawn(async move {
        let _ = uplink.run(uplink_rx, uplink_cancel).await;
    });

    // Feed: the SAME PcapFileSource as the local path, into the uplink's channel.
    let mut src_metrics_sys = MetricsSystem::new();
    let src_metrics = src_metrics_sys.register_worker(
        "capture.probe",
        &[
            Metric::CapturePacketsReceived,
            Metric::CaptureKernelPacketsDropped,
        ],
    );
    let _src_svc = src_metrics_sys.start();
    let source = PcapFileSource::new(path, "probe-diff".to_string(), None);
    let src_cancel = CancellationToken::new();
    let src_handle = tokio::spawn(async move {
        let _ = Box::new(source)
            .run(RoutingSender::single(uplink_tx), src_metrics, src_cancel)
            .await;
    });

    // Strict drain order (so every packet arrives before we collect):
    // 1. pcap EOF → src task ends → its RoutingSender (sole uplink_tx) drops.
    let _ = timeout(STEP_TIMEOUT, src_handle)
        .await
        .expect("pcap source drained");
    // 2. uplink_rx closes → uplink flushes its final partial batch + returns. NOTE
    //    the uplink returning only means every frame was WRITTEN TO THE SOCKET —
    //    not that the central has read them. Cancelling the central now would race
    //    its socket-drain and lose the unread tail.
    let _ = timeout(STEP_TIMEOUT, uplink_handle)
        .await
        .expect("uplink drained");
    // 3. Wait for the central to finish reading + forwarding the whole batch: poll
    //    its received-packet counter until it stops climbing (the probe's FIN lets
    //    handle_conn drain to EOF). Only then is it safe to tear the central down.
    let mut prev = u64::MAX;
    for _ in 0..200 {
        let cur = central_metrics_probe
            .counter(Metric::CapturePacketsReceived)
            .get();
        if cur == prev {
            break;
        }
        prev = cur;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 4. cancel the (drained) central → accept loop exits → its raw_tx drops.
    cancel.cancel();
    let _ = timeout(STEP_TIMEOUT, central_handle)
        .await
        .expect("central stopped");
    // 5. raw_rx closed → pipeline cascades closed → collectors finish.
    let out = timeout(Duration::from_secs(30), drain)
        .await
        .expect("pipeline drained");
    // A clean probe disconnect (graceful TLS close_notify) must not surface as a
    // read error at the central — else a probe restart would inflate the counter.
    assert_eq!(
        central_metrics_probe
            .counter(Metric::CaptureReadErrors)
            .get(),
        0,
        "clean probe disconnect raised a central read error"
    );
    Some(out)
}

fn project(out: &PipelineOutput) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let (turns, calls) = out;
    (
        common::sort_values(turns.iter().map(common::project_turn).collect()),
        common::sort_values(calls.iter().map(|c| common::project_call(c)).collect()),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distributed_path_is_byte_equivalent_to_local() {
    let manifest_src = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pcaps/corpus.toml"),
    )
    .expect("corpus.toml must exist");
    let manifest: toml::Value = toml::from_str(&manifest_src).expect("corpus.toml parses");
    let fixtures = manifest
        .get("fixture")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();

    let mut ran = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for fx in &fixtures {
        let id = fx
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let file = fx
            .get("file")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let status = fx
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("active");
        if status != "active" {
            skipped += 1;
            continue;
        }
        // Both paths require the fixture; skip together if absent / LFS pointer.
        let Some(local) = common::run_pcap_collecting_calls(&file).await else {
            eprintln!("skip {id}: fixture {file} not present");
            skipped += 1;
            continue;
        };
        let Some(dist) = run_distributed(&file).await else {
            skipped += 1;
            continue;
        };
        ran += 1;

        let (lt, lc) = project(&local);
        let (dt, dc) = project(&dist);
        if lt.len() != dt.len() || lc.len() != dc.len() {
            failures.push(format!(
                "{id}: count drift — local {} turns/{} calls vs distributed {} turns/{} calls",
                lt.len(),
                lc.len(),
                dt.len(),
                dc.len()
            ));
            continue;
        }
        if lt != dt {
            failures.push(format!(
                "{id}: TURNS differ between local and distributed paths"
            ));
        }
        if lc != dc {
            failures.push(format!(
                "{id}: CALLS differ between local and distributed paths"
            ));
        }
    }

    eprintln!("wire-equivalence: ran={ran} skipped={skipped}");
    assert!(
        failures.is_empty(),
        "the distributed (probe→wire→central) path is NOT byte-equivalent to local capture:\n{}",
        failures.join("\n")
    );
}
