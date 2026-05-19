//! Verifies the SSE streaming + capture path end-to-end:
//!
//! 1. A fake HTTPS upstream emits SSE chunks with 100ms gaps between
//!    them and a 1s gap before the final `[DONE]` frame.
//! 2. The client (curl-style raw socket) reads each chunk as it
//!    arrives. We assert the *gap between chunks observed by the
//!    client* matches the upstream's emission cadence — i.e., the
//!    proxy isn't buffering.
//! 3. After the stream ends, we assert the captured
//!    `HttpJoinerEvent::Exchange` contains the correctly parsed
//!    `SseEventData[]` matching every event the fake upstream sent.

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::Response;
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig as RustlsServerConfig};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use ts_proxy::{
    load_or_generate_ca, spawn_proxy, ProxyConfig, ProxyDeps, UpstreamClient,
};

/// Fake upstream that emits a fixed sequence of SSE frames with
/// configurable delays between them. Returns `(addr, cert_pem)`.
async fn spawn_sse_upstream() -> (SocketAddr, String) {
    let key_pair = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "sse-upstream");
    params.distinguished_name = dn;
    params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_pem = cert.pem();

    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls_cfg = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der())),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls_cfg));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let tls = match acceptor.accept(stream).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                let svc = service_fn(|_req| async move {
                    // 4 events; the gap between event 3 and event 4 is
                    // 500ms — chosen long enough to be unambiguous if the
                    // proxy buffers vs streams.
                    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(4);
                    tokio::spawn(async move {
                        for (i, payload) in [
                            "event: message_start\ndata: {\"role\":\"assistant\"}\n\n",
                            "event: content_block_delta\ndata: {\"delta\":{\"text\":\"hello\"}}\n\n",
                            "event: content_block_delta\ndata: {\"delta\":{\"text\":\" world\"}}\n\n",
                        ]
                        .iter()
                        .enumerate()
                        {
                            let _ = tx
                                .send(Ok(Frame::data(Bytes::from_static(payload.as_bytes()))))
                                .await;
                            // 100ms between the first three frames.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            let _ = i; // suppress unused
                        }
                        // 500ms gap, then final [DONE].
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let _ = tx
                            .send(Ok(Frame::data(Bytes::from_static(
                                b"event: message_stop\ndata: [DONE]\n\n",
                            ))))
                            .await;
                    });
                    let body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx))
                        .boxed_unsync();
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .header("cache-control", "no-cache")
                            .body(body)
                            .unwrap(),
                    )
                });
                let io = TokioIo::new(tls);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });

    (bound, cert_pem)
}

#[tokio::test]
async fn sse_streams_chunks_to_client_and_captures_parsed_events() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    // 1. SSE upstream.
    let (upstream_addr, upstream_cert_pem) = spawn_sse_upstream().await;
    let mut upstream_roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut upstream_cert_pem.as_bytes()) {
        upstream_roots
            .add(CertificateDer::from(cert.unwrap().to_vec()))
            .unwrap();
    }
    let upstream_client = UpstreamClient::with_roots(upstream_roots);

    // 2. Proxy.
    let ca_dir = TempDir::new().unwrap();
    let _ca = load_or_generate_ca(ca_dir.path()).unwrap();
    let ca_pem = std::fs::read_to_string(ca_dir.path().join("ca.pem")).unwrap();
    let (joiner_tx, mut joiner_rx) = mpsc::channel(8);
    let config = ProxyConfig {
        enabled: true,
        listen: "127.0.0.1".into(),
        port: 0,
        ca_dir: ca_dir.path().to_string_lossy().into_owned(),
        ..ProxyConfig::default()
    };
    let deps = ProxyDeps {
        joiner_event_tx: Some(joiner_tx),
        upstream: upstream_client,
    };
    let (_handle, proxy_addr) = spawn_proxy(config, deps).await.unwrap();

    // 3. Client trusts our MITM CA.
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(CertificateDer::from(cert.unwrap().to_vec())).unwrap();
    }
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    // 4. CONNECT.
    let mut sock = TcpStream::connect(proxy_addr).await.unwrap();
    let connect = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        upstream_addr.port(),
        upstream_addr.port()
    );
    sock.write_all(connect.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = sock.read(&mut buf).await.unwrap();
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));

    // 5. TLS handshake against the upgraded socket.
    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from("127.0.0.1").unwrap();
    let mut tls = connector.connect(server_name, sock).await.unwrap();

    // 6. POST a fake OpenAI Chat streaming request.
    let body = r#"{"model":"gpt-4","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer sk-test-1234567890abcdef\r\n\
         Content-Type: application/json\r\n\
         Accept: text/event-stream\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{}",
        body.len(),
        body
    );
    tls.write_all(req.as_bytes()).await.unwrap();
    tls.flush().await.unwrap();

    // 7. Time-stamp each read. The proxy is buffering if all chunks
    //    arrive together; it's streaming if the last chunk arrives
    //    ≥400ms after one of the earlier ones (we set a 500ms gap
    //    upstream — 400ms is the threshold below which we'd call it
    //    "no buffering observed").
    let mut chunks_with_arrival: Vec<(Instant, Vec<u8>)> = Vec::new();
    let mut all = Vec::new();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    let mut rbuf = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, tls.read(&mut rbuf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let now = Instant::now();
                let bytes = rbuf[..n].to_vec();
                all.extend_from_slice(&bytes);
                chunks_with_arrival.push((now, bytes));
            }
            _ => break,
        }
        if all.windows(6).any(|w| w == b"[DONE]") {
            break;
        }
    }
    let all_str = String::from_utf8_lossy(&all);

    // The HTTP response status line + headers come first; SSE frames
    // follow. Skip past the headers for sanity check.
    assert!(
        all_str.starts_with("HTTP/1.1 200"),
        "expected 200 OK, got: {all_str:?}"
    );
    assert!(
        all_str.contains("[DONE]"),
        "expected [DONE] in response, got: {all_str:?}"
    );

    // Verify streaming: the LAST chunk (with [DONE]) arrives at least
    // ~400ms after some earlier chunk. If the proxy buffered, all
    // chunks would arrive ~simultaneously (within a few ms).
    assert!(
        chunks_with_arrival.len() >= 2,
        "expected multiple chunks, got: {}",
        chunks_with_arrival.len()
    );
    let first_chunk_at = chunks_with_arrival[0].0;
    let last_chunk_at = chunks_with_arrival.last().unwrap().0;
    let gap = last_chunk_at.duration_since(first_chunk_at);
    assert!(
        gap >= Duration::from_millis(400),
        "expected streaming (≥400ms spread across chunks); proxy is buffering. gap={gap:?}, chunks={}",
        chunks_with_arrival.len()
    );

    // 8. Capture event lands with parsed SSE events.
    let event = tokio::time::timeout(Duration::from_secs(5), joiner_rx.recv())
        .await
        .expect("joiner event arrived")
        .expect("event present");
    match event {
        ts_protocol::joiner::HttpJoinerEvent::Exchange {
            request,
            response,
            sse_events,
            ..
        } => {
            assert_eq!(request.method, "POST");
            assert_eq!(response.status, 200);
            // Joiner contract: SSE bodies are not persisted (Bytes::new()).
            assert_eq!(response.body.len(), 0);
            // We sent 4 events upstream: message_start, two
            // content_block_deltas, message_stop.
            assert_eq!(
                sse_events.len(),
                4,
                "expected 4 parsed SSE events, got {}: {:?}",
                sse_events.len(),
                sse_events.iter().map(|e| (&e.event_type, &e.data)).collect::<Vec<_>>(),
            );
            assert_eq!(sse_events[0].event_type, "message_start");
            assert!(sse_events[1].data.contains("hello"));
            assert!(sse_events[2].data.contains("world"));
            assert_eq!(sse_events[3].event_type, "message_stop");
        }
        other => panic!("expected Exchange, got: {other:?}"),
    }

    // 9. The first body chunk should have arrived at the client well
    //    before the stream finished — i.e., TTFT-to-first-SSE-chunk is
    //    small (well under the 500ms upstream tail).
    let first_chunk_relative = chunks_with_arrival
        .iter()
        .find(|(_, bytes)| {
            // Strip past headers — first body chunk is the one that
            // contains an SSE frame marker.
            bytes.windows(6).any(|w| w == b"event:")
        })
        .map(|(t, _)| t.duration_since(start))
        .expect("at least one SSE body chunk seen");
    assert!(
        first_chunk_relative < Duration::from_millis(500),
        "TTFT to first SSE body chunk was {first_chunk_relative:?}, suggesting buffering"
    );
}
