//! End-to-end smoke test for the CONNECT + TLS-termination + inner
//! HTTP serve path. Validates that:
//!
//! 1. The proxy listener binds + accepts connections.
//! 2. A raw `CONNECT host:443` request is answered with 200.
//! 3. TLS handshake against the upgraded socket succeeds using a leaf
//!    cert minted on the fly for the SNI, signed by our generated CA.
//! 4. The decrypted inner HTTP/1.1 request makes it to the forwarder
//!    stub, which currently returns 503 "not wired yet".
//!
//! With the foundation verified, task #92 can replace the stub with the
//! real forwarder + capture logic.

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use ts_proxy::{load_or_generate_ca, spawn_proxy, ProxyConfig, ProxyDeps};

#[tokio::test]
async fn connect_then_tls_then_inner_request_reaches_forwarder() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    // 1. Pre-generate the CA into a tempdir so we can hand both the
    //    proxy and the test client the same trust anchor.
    let dir = TempDir::new().unwrap();
    let _ca = load_or_generate_ca(dir.path()).expect("ca");
    let ca_pem = std::fs::read_to_string(dir.path().join("ca.pem")).unwrap();

    // 2. Boot the proxy on port 0 against the same ca_dir so it picks
    //    up the same CA we just generated.
    let config = ProxyConfig {
        enabled: true,
        listen: "127.0.0.1".into(),
        port: 0,
        ca_dir: dir.path().to_string_lossy().into_owned(),
        ..ProxyConfig::default()
    };
    let (_handle, bound) = spawn_proxy(config, ProxyDeps::default())
        .await
        .expect("spawn");

    // 3. Build a rustls client config that trusts our generated CA.
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(CertificateDer::from(cert.unwrap().to_vec())).unwrap();
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    // 4. Open a plain TCP socket to the proxy and send a CONNECT.
    let mut sock = TcpStream::connect(bound).await.expect("connect");
    let connect_req = b"CONNECT api.openai.com:443 HTTP/1.1\r\nHost: api.openai.com:443\r\n\r\n";
    sock.write_all(connect_req).await.unwrap();
    sock.flush().await.unwrap();

    // 5. Read until we see the empty line that terminates the CONNECT
    //    response. The proxy returns `HTTP/1.1 200 OK\r\n\r\n`.
    let mut head = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = sock.read(&mut buf).await.unwrap();
        assert!(n > 0, "proxy closed before CONNECT response");
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head_str = String::from_utf8_lossy(&head);
    assert!(
        head_str.starts_with("HTTP/1.1 200"),
        "expected 200 OK, got: {head_str:?}"
    );

    // 6. TLS handshake against the now-upgraded socket.
    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from("api.openai.com").unwrap();
    let mut tls = connector
        .connect(server_name, sock)
        .await
        .expect("TLS handshake");

    // 7. Send an inner HTTP request. The forwarder stub returns 503.
    //    `Connection: close` so the server tears down the socket after
    //    the response — saves us from juggling Content-Length parsing
    //    in the smoke test.
    let inner_req = b"POST /v1/chat/completions HTTP/1.1\r\n\
                      Host: api.openai.com\r\n\
                      Content-Length: 0\r\n\
                      Authorization: Bearer sk-test-1234\r\n\
                      Connection: close\r\n\
                      \r\n";
    tls.write_all(inner_req).await.unwrap();
    tls.flush().await.unwrap();

    // 8. Read the inner response and confirm we hit the forwarder stub.
    //    5s deadline keeps a stuck server from wedging the test.
    let mut response = Vec::new();
    let mut rbuf = [0u8; 1024];
    let read_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = read_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, tls.read(&mut rbuf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => response.extend_from_slice(&rbuf[..n]),
            Ok(Err(_)) => break,
            Err(_) => break,
        }
        if response.len() > 4096 {
            break;
        }
    }
    let resp_str = String::from_utf8_lossy(&response);
    assert!(
        resp_str.starts_with("HTTP/1.1 503"),
        "expected stub 503, got: {resp_str:?}"
    );
    assert!(
        resp_str.contains("forwarder not yet wired"),
        "expected stub body, got: {resp_str:?}"
    );
}
