//! Hyper-based HTTP/1.1 proxy listener.
//!
//! Two request flows:
//! * `CONNECT host:port` → reply 200, upgrade the TCP socket, hand it
//!   to `tunnel::terminate_and_serve` which TLS-terminates and serves
//!   the decrypted HTTP/1.1 connection. This is the path used by
//!   essentially every `HTTPS_PROXY=...` client.
//! * Plain HTTP request with an absolute-form URI (e.g. `GET
//!   http://example.com/foo HTTP/1.1`) → for HTTP_PROXY (no TLS) use.
//!   For MVP we just reject these with 501; the focus is HTTPS capture,
//!   and plain-HTTP LLM endpoints are already sniffable upstream.

use crate::state::{ProxyDeps, ProxyState};
use crate::tls::LeafCertStore;
use crate::tunnel::{
    connect_response_bad_request, connect_response_ok, terminate_and_serve,
};
use crate::{load_or_generate_ca, ProxyConfig};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

#[derive(Debug, Error)]
pub enum ProxyServerError {
    #[error("CA load/generate failed: {0}")]
    Ca(#[from] crate::ca::CaError),
    #[error("bind {addr} failed: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid listen address {listen}:{port}: {source}")]
    InvalidAddress {
        listen: String,
        port: u16,
        #[source]
        source: std::net::AddrParseError,
    },
}

/// Spin up the proxy on a background task. Returns the JoinHandle and
/// the bound `SocketAddr` (useful when `port = 0` for tests).
pub async fn spawn_proxy(
    config: ProxyConfig,
    deps: ProxyDeps,
) -> Result<(JoinHandle<()>, SocketAddr), ProxyServerError> {
    let addr_str = format!("{}:{}", config.listen, config.port);
    let addr: SocketAddr =
        addr_str
            .parse()
            .map_err(|source| ProxyServerError::InvalidAddress {
                listen: config.listen.clone(),
                port: config.port,
                source,
            })?;

    let listener =
        TcpListener::bind(addr)
            .await
            .map_err(|source| ProxyServerError::Bind { addr, source })?;
    let bound = listener.local_addr().unwrap_or(addr);

    // Materialize CA + leaf store before we start serving. Doing this
    // before the accept loop means CA-generation errors surface as a
    // startup error rather than a per-connection failure.
    let ca = load_or_generate_ca(Path::new(&config.ca_dir))?;
    let leaf_store = Arc::new(LeafCertStore::new(ca));

    let state = Arc::new(ProxyState {
        config,
        leaf_store,
        deps,
    });

    info!(target: "ts_proxy::server", addr = %bound, "proxy listening");

    let handle = tokio::spawn(accept_loop(listener, state));
    Ok((handle, bound))
}

async fn accept_loop(listener: TcpListener, state: Arc<ProxyState>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let st = state.clone();
                tokio::spawn(async move {
                    serve_connection(stream, peer, st).await;
                });
            }
            Err(e) => {
                // Most accept errors are recoverable (per-connection
                // resource exhaustion); log and retry. A persistent
                // failure here usually means the listening fd is gone,
                // in which case the next .accept will return the same
                // error and the operator will see the log spam.
                error!(target: "ts_proxy::server", error = %e, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    state: Arc<ProxyState>,
) {
    debug!(target: "ts_proxy::server", peer = %peer, "new client");

    let svc_state = state.clone();
    let service = service_fn(move |req: Request<Incoming>| {
        let st = svc_state.clone();
        async move { route_top_level(req, st).await }
    });

    let io = TokioIo::new(stream);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .serve_connection(io, service)
        .with_upgrades()
        .await
    {
        // Drops to debug — most "errors" here are clients closing
        // mid-conversation or after a CONNECT upgrade (expected once
        // we hand the socket to the tunnel task).
        debug!(target: "ts_proxy::server", peer = %peer, error = %e, "client connection ended");
    }
}

async fn route_top_level(
    req: Request<Incoming>,
    state: Arc<ProxyState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.method() == Method::CONNECT {
        return Ok(handle_connect(req, state).await);
    }
    Ok(handle_plain_http(req).await)
}

fn handle_connect(req: Request<Incoming>, state: Arc<ProxyState>) -> impl std::future::Future<Output = Response<Full<Bytes>>> + Send {
    async move {
        let authority = match req.uri().authority().cloned() {
            Some(a) => a,
            None => {
                warn!(target: "ts_proxy::server", uri = %req.uri(), "CONNECT without authority");
                return connect_response_bad_request("CONNECT requires authority");
            }
        };
        let host = authority.host().to_string();

        // Schedule the TLS termination on a fresh task. hyper handles
        // the upgrade once we return 200 — the upgraded socket gets
        // resolved by the `on(req)` future.
        let st = state.clone();
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let io = TokioIo::new(upgraded);
                    terminate_and_serve(io, host, st).await;
                }
                Err(e) => {
                    warn!(target: "ts_proxy::server", error = %e, "upgrade on CONNECT failed");
                }
            }
        });
        connect_response_ok()
    }
}

async fn handle_plain_http(req: Request<Incoming>) -> Response<Full<Bytes>> {
    // For MVP we don't handle absolute-form plain HTTP through the
    // proxy. Most LLM endpoints are HTTPS, and the existing sniffer
    // already captures cleartext HTTP at the NIC layer. Returning 501
    // keeps the proxy honest about its scope.
    let body = format!(
        r#"{{"error":"tokenscope proxy: plain HTTP forwarding not implemented; this proxy only handles CONNECT (HTTPS_PROXY).","method":"{}","uri":"{}"}}"#,
        req.method(),
        req.uri()
    );
    Response::builder()
        .status(501)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("static response")
}
