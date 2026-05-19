//! Inner request handling: parse, classify, forward upstream, capture.
//!
//! For each inner HTTP request received over the MITM-decrypted tunnel:
//!
//! 1. Buffer the full request body up to `config.max_body_bytes`.
//! 2. Build an `HttpRequestData` and run `WireApiRegistry::detect`.
//!    If it doesn't match any wire API and `allow_passthrough = false`,
//!    return 403 immediately and skip forwarding.
//! 3. Forward upstream over `UpstreamClient`. The Host header and URI
//!    are rewritten to the CONNECT authority.
//! 4. Collect the upstream response body, build an `HttpResponseData`
//!    (and SSE events if the response was `text/event-stream`), then
//!    fire-and-forget submit the `HttpJoinerEvent::Exchange` to the
//!    capture sink.
//! 5. Return the response to the inner-HTTP client.
//!
//! v1 buffers all bodies (including SSE) — adequate for proving the
//! capture path end-to-end, but adds latency for streaming clients.
//! A follow-up will replace SSE with a tee'd `BoxBody` so TTFT
//! mirrors a direct connection.

use crate::capture::{
    make_flow_key, make_request_data, make_response_data, parse_sse_chunk, submit_exchange,
};
use crate::redact::redact_headers;
use crate::state::{ProxyState, TunnelContext};
use bytes::Bytes;
use http::HeaderName;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, Uri};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};
use ts_llm::ParsedJson;

/// `Content-Length` cap honored by the body collector. Beyond this we
/// reject up front to keep memory usage bounded — matches the
/// joiner-side cap from `ProxyConfig::max_body_bytes`.
pub async fn handle_inner_request(
    req: Request<Incoming>,
    ctx: TunnelContext,
    state: Arc<ProxyState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let started_us = now_us();
    match forward_and_capture(req, ctx, state, started_us).await {
        Ok(resp) => Ok(resp),
        Err(reason) => Ok(reason.into_response()),
    }
}

async fn forward_and_capture(
    req: Request<Incoming>,
    ctx: TunnelContext,
    state: Arc<ProxyState>,
    started_us: i64,
) -> Result<Response<Full<Bytes>>, ForwardError> {
    let max_body = state.config.max_body_bytes;
    let (req_parts, req_body) = req.into_parts();
    let req_body_bytes = collect_capped(req_body, max_body).await?;

    let captured_request = build_captured_request(&req_parts, &req_body_bytes, &ctx, &state, started_us);

    let detection = state
        .registry
        .detect(&captured_request, &ParsedJson::from_bytes(req_body_bytes.clone()));
    let wire_api_name = detection.as_ref().map(|d| d.wire_api.name().to_string());

    if detection.is_none() && !state.config.allow_passthrough {
        info!(
            target: "ts_proxy::forward",
            host = %ctx.host,
            path = req_parts.uri.path(),
            "non-LLM request rejected"
        );
        return Err(ForwardError::NotAnLlmRequest);
    }

    drop(detection); // releases the &registry borrow; we keep wire_api_name only

    // Build the upstream URI from the CONNECT authority + the inner
    // request's path-and-query. Inner clients send origin-form URIs
    // (just `/v1/...`) because they think they're talking to the
    // origin server directly, so we have to reattach scheme + host.
    let upstream_uri = build_upstream_uri(&ctx, &req_parts.uri)?;
    let upstream_req = build_upstream_request(&req_parts, req_body_bytes.clone(), &upstream_uri)?;

    debug!(
        target: "ts_proxy::forward",
        host = %ctx.host,
        wire_api = %wire_api_name.as_deref().unwrap_or("(passthrough)"),
        "forwarding upstream"
    );

    let first_byte_us;
    let upstream_resp = match state.deps.upstream.send(upstream_req).await {
        Ok(r) => {
            first_byte_us = now_us();
            r
        }
        Err(e) => {
            // hyper-util's `Error::Connect` is opaque (just "client error
            // (Connect)") — its real cause sits in the .source() chain.
            // Surface the whole chain so misconfigured trust stores,
            // TLS handshake failures, and DNS errors are diagnosable
            // from logs alone.
            let chain: Vec<String> = std::iter::successors(
                Some(&e as &(dyn std::error::Error + 'static)),
                |err| err.source(),
            )
            .map(|err| err.to_string())
            .collect();
            warn!(
                target: "ts_proxy::forward",
                host = %ctx.host,
                error = %e,
                chain = ?chain,
                "upstream send failed"
            );
            return Err(ForwardError::Upstream(chain.join(": ")));
        }
    };

    let (resp_parts, resp_body) = upstream_resp.into_parts();
    let resp_body_bytes = collect_capped(resp_body, max_body).await?;
    let complete_us = now_us();

    // Build the captured response. SSE bodies are not persisted —
    // the existing joiner contract is `body = Bytes::new()` for SSE.
    let is_sse = resp_parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/event-stream"))
        .unwrap_or(false);

    let captured_response = build_captured_response(
        &resp_parts,
        if is_sse {
            Bytes::new()
        } else {
            resp_body_bytes.clone()
        },
        &captured_request,
        first_byte_us,
        complete_us,
    );

    let sse_events = if is_sse {
        let raw = std::str::from_utf8(&resp_body_bytes).unwrap_or("");
        parse_sse_chunk(
            &captured_response.flow_key,
            client_socket(&ctx),
            upstream_socket(&ctx),
            raw,
            first_byte_us,
        )
    } else {
        Vec::new()
    };

    if let Some(tx) = state.deps.joiner_event_tx.as_ref() {
        submit_exchange(tx, captured_request, captured_response, sse_events).await;
    } else {
        debug!(target: "ts_proxy::forward", host = %ctx.host, "no joiner sink configured; capture dropped");
    }

    Ok(rebuild_client_response(resp_parts, resp_body_bytes))
}

/// Read a body up to `max` bytes, then either yield it or fail with
/// `ForwardError::PayloadTooLarge`. We pull frame-by-frame so a stream
/// that exceeds the cap can be aborted early rather than buffered fully
/// just to be rejected.
async fn collect_capped<B>(body: B, max: usize) -> Result<Bytes, ForwardError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    use http_body_util::BodyExt;
    let mut body = body;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| ForwardError::Upstream(format!("body frame: {e}")))?;
        if let Ok(data) = frame.into_data() {
            if buf.len() + data.len() > max {
                return Err(ForwardError::PayloadTooLarge { cap: max });
            }
            buf.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(buf))
}

fn build_captured_request(
    req_parts: &http::request::Parts,
    body: &Bytes,
    ctx: &TunnelContext,
    state: &ProxyState,
    timestamp_us: i64,
) -> ts_protocol::model::HttpRequestData {
    let mut headers: Vec<(String, String)> = req_parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    redact_headers(&mut headers, state.config.redact_api_keys);

    let method = req_parts.method.as_str().to_string();
    let uri = req_parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req_parts.uri.path().to_string());
    let version_minor = match req_parts.version {
        http::Version::HTTP_10 => 0,
        _ => 1,
    };

    let flow_key = make_flow_key(
        &state.config.source_id,
        ctx.client_peer,
        &ctx.host,
        ctx.port,
    );
    make_request_data(
        flow_key,
        client_socket(ctx),
        upstream_socket(ctx),
        method,
        uri,
        version_minor,
        headers,
        body.clone(),
        timestamp_us,
    )
}

fn build_captured_response(
    resp_parts: &http::response::Parts,
    body: Bytes,
    captured_request: &ts_protocol::model::HttpRequestData,
    first_byte_us: i64,
    complete_us: i64,
) -> ts_protocol::model::HttpResponseData {
    let headers: Vec<(String, String)> = resp_parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let version_minor = match resp_parts.version {
        http::Version::HTTP_10 => 0,
        _ => 1,
    };
    make_response_data(
        captured_request.flow_key.clone(),
        SocketAddr::new(captured_request.client_addr.0, captured_request.client_addr.1),
        SocketAddr::new(captured_request.server_addr.0, captured_request.server_addr.1),
        resp_parts.status.as_u16(),
        version_minor,
        headers,
        body,
        first_byte_us,
        complete_us,
    )
}

fn build_upstream_uri(ctx: &TunnelContext, inner_uri: &Uri) -> Result<Uri, ForwardError> {
    let path_and_query = inner_uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let raw = format!("https://{}:{}{}", ctx.host, ctx.port, path_and_query);
    raw.parse::<Uri>()
        .map_err(|e| ForwardError::Upstream(format!("invalid upstream URI: {e}")))
}

fn build_upstream_request(
    req_parts: &http::request::Parts,
    body: Bytes,
    upstream_uri: &Uri,
) -> Result<Request<Full<Bytes>>, ForwardError> {
    let mut builder = Request::builder()
        .method(req_parts.method.clone())
        .uri(upstream_uri.clone());

    // Forward all headers verbatim except hop-by-hop ones (which would
    // confuse upstream if we relayed them). Host header gets rewritten
    // to the upstream authority — the client's Host points at the
    // origin behind the proxy (same thing for a CONNECT-targeted SDK,
    // but we re-set for safety).
    let hop_by_hop: &[HeaderName] = &[
        http::header::CONNECTION,
        http::header::PROXY_AUTHENTICATE,
        http::header::PROXY_AUTHORIZATION,
        http::header::TE,
        http::header::TRAILER,
        http::header::TRANSFER_ENCODING,
        http::header::UPGRADE,
        http::header::HOST,
    ];

    if let Some(headers_mut) = builder.headers_mut() {
        for (name, value) in req_parts.headers.iter() {
            if hop_by_hop.iter().any(|h| h == name) {
                continue;
            }
            headers_mut.append(name.clone(), value.clone());
        }
        headers_mut.insert(
            http::header::HOST,
            upstream_uri
                .authority()
                .map(|a| {
                    http::HeaderValue::from_str(a.as_str())
                        .unwrap_or_else(|_| http::HeaderValue::from_static(""))
                })
                .unwrap_or_else(|| http::HeaderValue::from_static("")),
        );
    }

    builder
        .body(Full::new(body))
        .map_err(|e| ForwardError::Upstream(format!("build upstream request: {e}")))
}

fn rebuild_client_response(
    resp_parts: http::response::Parts,
    body: Bytes,
) -> Response<Full<Bytes>> {
    // Skip hop-by-hop response headers per RFC 7230 §6.1. Most
    // matter only for chunked transport, but stripping them keeps
    // strict clients (e.g. nodejs http parsers) happy when we
    // re-frame with Content-Length.
    let strip: &[HeaderName] = &[
        http::header::CONNECTION,
        http::header::TRANSFER_ENCODING,
        http::header::TRAILER,
        http::header::UPGRADE,
    ];
    let mut builder = Response::builder()
        .status(resp_parts.status)
        .version(resp_parts.version);
    if let Some(hs) = builder.headers_mut() {
        for (name, value) in resp_parts.headers.iter() {
            if strip.iter().any(|h| h == name) {
                continue;
            }
            hs.append(name.clone(), value.clone());
        }
    }
    builder
        .body(Full::new(body))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(502)
                .body(Full::new(Bytes::from_static(b"tokenscope: response rebuild failed")))
                .expect("static")
        })
}

fn client_socket(ctx: &TunnelContext) -> SocketAddr {
    ctx.client_peer
}

fn upstream_socket(ctx: &TunnelContext) -> SocketAddr {
    // We don't have a real resolved IP for the upstream — `capture.rs`
    // uses LOCALHOST as a stand-in. For now just synthesize one from
    // the host string when it parses as an IP, falling back to
    // 0.0.0.0:port otherwise. The displayed value is purely for the
    // UI; transport semantics don't depend on it.
    let ip = ctx
        .host
        .parse()
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    SocketAddr::new(ip, ctx.port)
}

fn now_us() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

enum ForwardError {
    NotAnLlmRequest,
    Upstream(String),
    PayloadTooLarge { cap: usize },
}

impl ForwardError {
    fn into_response(self) -> Response<Full<Bytes>> {
        match self {
            ForwardError::NotAnLlmRequest => json_response(
                403,
                r#"{"error":"tokenscope proxy: request does not look like an LLM call; set proxy.allow_passthrough=true to bypass this filter"}"#,
            ),
            ForwardError::Upstream(reason) => json_response(
                502,
                &format!(
                    r#"{{"error":"tokenscope proxy: upstream failure","detail":"{}"}}"#,
                    escape_json(&reason)
                ),
            ),
            ForwardError::PayloadTooLarge { cap } => json_response(
                413,
                &format!(
                    r#"{{"error":"tokenscope proxy: payload exceeded max_body_bytes","cap":{cap}}}"#
                ),
            ),
        }
    }
}

fn json_response(status: u16, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("server", "tokenscope-proxy/0.2")
        .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
        .expect("static response")
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
