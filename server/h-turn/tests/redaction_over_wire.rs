//! Edge redaction over the wire.
//!
//! Two guarantees for the distributed topology's defence-in-depth scrubber:
//!   (a) a secret a probe redacts NEVER reaches the central over the wire, while
//!       the prefix/header name survive and the byte length is unchanged;
//!   (b) redaction is transparent to extraction — on the (already scrubbed)
//!       corpus, the probe-with-redaction path produces the SAME turns/calls as
//!       a local run (equal-length scrubbing doesn't corrupt framing/parsing).

mod common;

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use h_capture::ebpf::redact::Redactor;
use h_capture::testpki::{gen_pki, pick_free_port, write_pem};
use h_capture::{CaptureSource, ProbeUplink, RawPacket, RoutingSender, ThinProbeSource};
use h_common::config::{TlsClientConfig, TlsServerConfig};
use h_common::internal_metrics::MetricsSystem;

const SECRET: &str = "TOPSECRET-abc123XYZ";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A plaintext HTTP request packet carrying the secret two ways — in the
/// `Authorization` header (whole value masked) and as an `sk-` token in the JSON
/// body (token chars masked, `sk-` prefix kept) — to exercise both redaction
/// modes (the bytes a probe would capture before TLS-encrypting them).
fn secret_request_packet() -> RawPacket {
    let payload = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\n\
         Authorization: Bearer sk-ant-{SECRET}\r\nContent-Type: application/json\r\n\r\n\
         {{\"api_key\":\"sk-live-{SECRET}\",\"model\":\"claude-3\"}}"
    );
    RawPacket {
        timestamp_us: 1_000_000,
        caplen: payload.len() as u32,
        wirelen: payload.len() as u32,
        link_type: 1,
        data: payload.into_bytes().into(),
        source_id: "probe".to_string(),
        process: None,
    }
}

/// Ship one packet (optionally redacted probe-side) through ProbeUplink → mTLS →
/// ThinProbeSource and return the concatenated bytes the central delivered.
async fn deliver(pkt: RawPacket, redact: bool) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let pki = gen_pki("probe");
    let ca = write_pem(dir.path(), "ca.pem", &pki.ca_pem);
    let server_crt = write_pem(dir.path(), "server.crt", &pki.server_cert_pem);
    let server_key = write_pem(dir.path(), "server.key", &pki.server_key_pem);
    let client_crt = write_pem(dir.path(), "client.crt", &pki.client_cert_pem);
    let client_key = write_pem(dir.path(), "client.key", &pki.client_key_pem);
    let port = pick_free_port();

    // Central taps a plain receiver (no pipeline — we scan the delivered bytes).
    let (tap_tx, mut tap_rx) = mpsc::channel::<RawPacket>(64);
    let cancel = CancellationToken::new();
    let mut central_metrics_sys = MetricsSystem::new();
    let central_metrics =
        central_metrics_sys.register_worker("thin-probe", common::THIN_PROBE_METRICS);
    let _central_svc = central_metrics_sys.start();
    let server_tls = TlsServerConfig {
        cert: server_crt,
        key: server_key,
        client_ca: ca.clone(),
    };
    let central =
        ThinProbeSource::from_config(format!("127.0.0.1:{port}"), &server_tls, None).unwrap();
    let central_cancel = cancel.clone();
    let central_handle = tokio::spawn(async move {
        let _ = Box::new(central)
            .run(
                RoutingSender::single(tap_tx),
                central_metrics,
                central_cancel,
            )
            .await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Probe redacts the plaintext before it leaves the host.
    let mut pkt = pkt;
    if redact {
        let mut data = pkt.data.to_vec();
        Redactor::with_defaults().redact(&mut data);
        pkt.data = data.into();
    }

    let client_tls = TlsClientConfig {
        cert: client_crt,
        key: client_key,
        server_ca: ca,
    };
    let uplink = ProbeUplink::from_config(
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        "probe".to_string(),
        &client_tls,
    )
    .unwrap()
    .with_batching(1, Duration::from_millis(10));
    let (uplink_tx, uplink_rx) = mpsc::channel::<RawPacket>(8);
    let uplink_cancel = cancel.clone();
    let uplink_handle = tokio::spawn(async move {
        let _ = uplink.run(uplink_rx, uplink_cancel).await;
    });
    uplink_tx.send(pkt).await.unwrap();
    drop(uplink_tx); // EOF → uplink ships + gracefully closes
    let _ = timeout(Duration::from_secs(15), uplink_handle).await;

    // Drain delivered packets (recv-timeout naturally waits for the central to
    // forward, then ends when no more arrive).
    let mut delivered = Vec::new();
    while let Ok(Some(p)) = timeout(Duration::from_millis(500), tap_rx.recv()).await {
        delivered.extend_from_slice(&p.data);
    }
    cancel.cancel();
    let _ = timeout(Duration::from_secs(15), central_handle).await;
    delivered
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_never_crosses_the_wire() {
    let original_len = secret_request_packet().data.len();

    let redacted = deliver(secret_request_packet(), true).await;
    assert!(!redacted.is_empty(), "the packet must be delivered");
    assert!(
        !contains(&redacted, SECRET.as_bytes()),
        "redacted: the API key must NOT cross the wire"
    );
    // Defence-in-depth visibility: the header name + the body's `sk-` prefix
    // survive (the Authorization value is fully masked; the body token keeps its
    // prefix with only the secret chars masked).
    assert!(contains(&redacted, b"Authorization:"), "header name stays");
    assert!(
        contains(&redacted, b"sk-"),
        "the body sk- prefix stays visible"
    );
    // Equal-length over the wire (Content-Length / offsets preserved).
    assert_eq!(
        redacted.len(),
        original_len,
        "redaction must be equal-length"
    );

    // Control: WITHOUT redaction the secret DOES cross — proves the test has teeth.
    let plain = deliver(secret_request_packet(), false).await;
    assert!(
        contains(&plain, SECRET.as_bytes()),
        "control: an unredacted secret must reach the central (else the test is vacuous)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redaction_is_transparent_to_extraction() {
    // On the scrubbed corpus, equal-length redaction must not change what the
    // pipeline extracts: probe-with-redaction == local.
    let mut ran = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (id, file) in common::active_fixtures() {
        let Some(local) = common::run_pcap_collecting_calls(&file).await else {
            continue;
        };
        let Some(dist) = common::run_distributed(&file, true).await else {
            continue;
        };
        ran += 1;
        if common::project(&local) != common::project(&dist) {
            failures.push(format!("{id}: redaction changed the extracted turns/calls"));
        }
    }
    eprintln!("redaction-transparency: ran={ran}");
    assert!(
        failures.is_empty(),
        "edge redaction is NOT transparent to extraction on scrubbed traffic:\n{}",
        failures.join("\n")
    );
}
