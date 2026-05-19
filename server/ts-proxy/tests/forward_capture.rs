//! End-to-end test of the forwarder + capture path.
//!
//! Spins up a fake HTTPS upstream (rustls + hyper) signed by a
//! throwaway cert, hands the proxy a RootCertStore containing that
//! cert, and verifies that a CONNECT → MITM-decrypt → forward upstream
//! → response back to client → HttpJoinerEvent::Exchange round-trip
//! captures everything the storage pipeline needs.

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig as RustlsServerConfig};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use ts_proxy::{
    load_or_generate_ca, spawn_proxy, ProxyConfig, ProxyDeps, UpstreamClient,
};

/// Spin up a single-shot HTTPS server on 127.0.0.1:0 that serves a
/// fixed JSON response. Returns (bound addr, cert PEM, an Arc<Mutex<>>
/// holding the request body the server saw — for assertions).
async fn spawn_fake_upstream() -> (SocketAddr, String, Arc<Mutex<Option<Vec<u8>>>>) {
    // Generate self-signed cert with SAN = 127.0.0.1
    let key_pair = KeyPair::generate().expect("key");
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "fake-upstream");
    params.distinguished_name = dn;
    params.subject_alt_names = vec![SanType::IpAddress(
        "127.0.0.1".parse().expect("valid ip"),
    )];
    let cert = params.self_signed(&key_pair).expect("self-sign");
    let cert_pem = cert.pem();
    let key_der = key_pair.serialize_der();

    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls_cfg = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
        )
        .expect("tls cfg");
    let acceptor = TlsAcceptor::from(Arc::new(tls_cfg));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();

    let observed_body = Arc::new(Mutex::new(None::<Vec<u8>>));
    let observed_clone = observed_body.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _peer) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let captured = observed_clone.clone();
            tokio::spawn(async move {
                let tls = match acceptor.accept(stream).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                let captured_inner = captured.clone();
                let svc = service_fn(move |req: Request<Incoming>| {
                    let cap = captured_inner.clone();
                    async move {
                        let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                        *cap.lock().unwrap() = Some(bytes);
                        let body = r#"{"id":"chatcmpl-test","choices":[{"message":{"role":"assistant","content":"hello from fake upstream"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}"#;
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from_static(body.as_bytes())))
                                .unwrap(),
                        )
                    }
                });
                let io = TokioIo::new(tls);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });

    (bound, cert_pem, observed_body)
}

#[tokio::test]
async fn openai_chat_through_proxy_captures_exchange() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    // 1. Fake upstream.
    let (upstream_addr, upstream_cert_pem, observed_request) = spawn_fake_upstream().await;
    let mut upstream_roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut upstream_cert_pem.as_bytes()) {
        upstream_roots
            .add(CertificateDer::from(cert.unwrap().to_vec()))
            .unwrap();
    }
    let upstream_client = UpstreamClient::with_roots(upstream_roots);

    // 2. Proxy.
    let ca_dir = TempDir::new().unwrap();
    let _ca = load_or_generate_ca(ca_dir.path()).expect("ca");
    let ca_pem = std::fs::read_to_string(ca_dir.path().join("ca.pem")).unwrap();

    let (joiner_tx, mut joiner_rx) = tokio::sync::mpsc::channel(8);
    let deps = ProxyDeps {
        joiner_event_tx: Some(joiner_tx),
        upstream: upstream_client,
    };
    let config = ProxyConfig {
        enabled: true,
        listen: "127.0.0.1".into(),
        port: 0,
        ca_dir: ca_dir.path().to_string_lossy().into_owned(),
        ..ProxyConfig::default()
    };
    let (_handle, proxy_addr) = spawn_proxy(config, deps).await.expect("spawn proxy");

    // 3. Build client that trusts our MITM CA.
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(CertificateDer::from(cert.unwrap().to_vec())).unwrap();
    }
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    // 4. CONNECT to 127.0.0.1:upstream_addr.port() through the proxy.
    //    The proxy will then forward the inner request to the same
    //    address over its upstream client.
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
    assert!(
        String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"),
        "CONNECT not accepted: {}",
        String::from_utf8_lossy(&head)
    );

    // 5. TLS handshake against the upgraded socket.
    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from("127.0.0.1").unwrap();
    let mut tls = connector.connect(server_name, sock).await.unwrap();

    // 6. Send an OpenAI Chat Completions request.
    let body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#;
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer sk-test-1234567890abcdef\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{}",
        body.len(),
        body
    );
    tls.write_all(req.as_bytes()).await.unwrap();
    tls.flush().await.unwrap();

    // 7. Read response.
    let mut resp = Vec::new();
    let mut rbuf = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, tls.read(&mut rbuf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => resp.extend_from_slice(&rbuf[..n]),
            _ => break,
        }
        if resp.len() > 8192 {
            break;
        }
    }
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(
        resp_str.starts_with("HTTP/1.1 200"),
        "expected 200, got: {resp_str}"
    );
    assert!(
        resp_str.contains("hello from fake upstream"),
        "expected fake upstream's body, got: {resp_str}"
    );

    // 8. Fake upstream observed the exact request body.
    let observed = observed_request.lock().unwrap().clone();
    assert_eq!(observed.as_deref(), Some(body.as_bytes()));

    // 9. The joiner channel received a captured Exchange.
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), joiner_rx.recv())
        .await
        .expect("joiner event arrived in time")
        .expect("joiner event present");
    match event {
        ts_protocol::joiner::HttpJoinerEvent::Exchange {
            request,
            response,
            sse_events,
            ..
        } => {
            assert_eq!(request.method, "POST");
            assert_eq!(request.uri, "/v1/chat/completions");
            assert_eq!(request.flow_key.source_id, "builtin-proxy");
            // Body is preserved verbatim — wire-api detection downstream
            // sees the same JSON the client sent.
            assert_eq!(request.body.as_ref(), body.as_bytes());
            // Authorization header is redacted (default MaskMiddle).
            let auth = request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            assert!(
                auth.starts_with("Bearer ") && auth.contains("***"),
                "authorization not redacted: {auth:?}"
            );
            assert!(
                !auth.contains("sk-test-1234567890abcdef"),
                "full secret leaked through: {auth:?}"
            );
            assert_eq!(response.status, 200);
            assert!(
                response.body.windows(20).any(|w| w == b"hello from fake upst") ||
                    response.body.windows(15).any(|w| w == b"hello from fake"),
                "captured response body doesn't contain upstream payload: {:?}",
                std::str::from_utf8(&response.body).unwrap_or("(non-utf8)")
            );
            assert!(sse_events.is_empty(), "no SSE expected on a JSON response");
        }
        other => panic!("expected Exchange, got: {other:?}"),
    }
}
