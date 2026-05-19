//! TLS termination and inner-HTTP serving for upgraded CONNECT
//! tunnels. The flow:
//!
//! 1. CONNECT handler in `server.rs` schedules `terminate_and_serve`
//!    on a fresh task and returns `200 OK` so hyper does the upgrade.
//! 2. We receive the raw upgraded socket, run a rustls handshake
//!    against it using a leaf cert minted for the requested host.
//! 3. The decrypted stream is then handed to *another* hyper HTTP/1
//!    `serve_connection`, this time dispatching to `forward.rs` which
//!    parses the inner request, classifies it, and proxies it upstream.

use crate::state::ProxyState;
use crate::SniResolver;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::Response;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

/// Runs after the CONNECT response has been sent. Wraps the upgraded
/// socket in rustls + hyper-server. Errors are logged but never
/// propagated — by the time we're here, the CONNECT response has
/// already flushed to the client, so there's nothing useful to return.
pub async fn terminate_and_serve<I>(upgraded: I, host: String, state: Arc<ProxyState>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let server_config = build_server_config(state.clone());
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let tls_stream = match acceptor.accept(upgraded).await {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "ts_proxy::tunnel", host = %host, error = %e, "tls handshake failed");
            return;
        }
    };
    debug!(target: "ts_proxy::tunnel", host = %host, "tls handshake ok");

    let inner_state = state.clone();
    let inner_host = host.clone();
    let service = service_fn(move |req| {
        let st = inner_state.clone();
        let host = inner_host.clone();
        async move { crate::forward::handle_inner_request(req, host, st).await }
    });

    let io = TokioIo::new(tls_stream);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .serve_connection(io, service)
        .await
    {
        // hyper logs the typical incomplete-message error for stream
        // tear-down at debug level — most "errors" here are just clients
        // closing connections, not real problems.
        debug!(target: "ts_proxy::tunnel", host = %host, error = %e, "inner http connection closed");
    }
}

fn build_server_config(state: Arc<ProxyState>) -> ServerConfig {
    let resolver: Arc<dyn tokio_rustls::rustls::server::ResolvesServerCert> =
        Arc::new(SniResolver::new(state.leaf_store.clone()));
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    // ALPN: advertise only http/1.1 for now. h2 between client and
    // proxy is a phase-2 — would require us to run http2 on the
    // decrypted stream too, and most LLM SDKs are happy with h1.
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg
}

/// Builder for the empty `200 OK` response we send back to the client
/// in answer to a CONNECT request, signaling that the tunnel is up.
pub fn connect_response_ok() -> Response<Full<Bytes>> {
    Response::builder()
        .status(200)
        .body(Full::new(Bytes::new()))
        .expect("static response")
}

/// Builder for the simple JSON error response used when CONNECT
/// targets a host we can't proxy (e.g. malformed authority).
pub fn connect_response_bad_request(reason: &str) -> Response<Full<Bytes>> {
    let body = format!("{{\"error\":\"tokenscope proxy: {reason}\"}}");
    Response::builder()
        .status(400)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("static response")
}
