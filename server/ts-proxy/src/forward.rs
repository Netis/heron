//! Inner request handling: parse, classify, forward upstream, tee
//! response, and submit a captured `HttpJoinerEvent::Exchange` into
//! the pipeline.
//!
//! This module is a stub for now (returns 503 with a "not yet wired"
//! message). Task #92 will fill it in with the real forwarder + capture
//! logic. Keeping it stubbed lets the CONNECT + TLS + inner-serve
//! plumbing in `server.rs` and `tunnel.rs` be smoke-tested independently.

use crate::state::ProxyState;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::info;

pub async fn handle_inner_request(
    req: Request<Incoming>,
    host: String,
    _state: Arc<ProxyState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    info!(
        target: "ts_proxy::forward",
        host = %host,
        method = %req.method(),
        path = req.uri().path(),
        "inner request received (forwarder not yet wired)"
    );

    let body = format!(
        r#"{{"error":"tokenscope proxy: forwarder not yet wired","host":"{}","method":"{}","path":"{}"}}"#,
        host,
        req.method(),
        req.uri().path()
    );

    Ok(Response::builder()
        .status(503)
        .header("content-type", "application/json")
        .header("server", "tokenscope-proxy/0.2 (mvp-scaffold)")
        .body(Full::new(Bytes::from(body)))
        .expect("static response"))
}
