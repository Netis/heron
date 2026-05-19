//! Inner request handling: parse, classify, forward upstream, capture.
//!
//! For each inner HTTP request received over the MITM-decrypted tunnel:
//!
//! 1. Buffer the full request body up to `config.max_body_bytes`.
//! 2. Build an `HttpRequestData` and run `WireApiRegistry::detect`.
//!    If it doesn't match any wire API and `allow_passthrough = false`,
//!    return 403 immediately and skip forwarding.
//! 3. Forward upstream over `UpstreamClient`.
//! 4. Branch on `Content-Type: text/event-stream`:
//!    - **SSE**: wrap the upstream body in `CapturingBody` and hand it
//!      to the client *immediately* — every frame flushes to the
//!      client as it arrives upstream, matching direct-connection
//!      TTFT. The capture event is submitted on stream end (or client
//!      tear-down) via the `Finalizer` callback.
//!    - **non-SSE**: buffer fully (LLM bodies are small) and submit
//!      capture before returning.
//! 5. Return the response to the inner-HTTP client.

use crate::body::CapturingBody;
use crate::capture::{
    make_flow_key, make_request_data, make_response_data, parse_sse_chunk, submit_exchange,
};
use crate::redact::redact_headers;
use crate::state::{ProxyState, TunnelContext};
use crate::ResponseBody;
use bytes::Bytes;
use http::HeaderName;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, Uri};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};
use ts_llm::ParsedJson;

pub async fn handle_inner_request(
    req: Request<Incoming>,
    ctx: TunnelContext,
    state: Arc<ProxyState>,
) -> Result<Response<ResponseBody>, Infallible> {
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
) -> Result<Response<ResponseBody>, ForwardError> {
    let max_body = state.config.max_body_bytes;
    let (req_parts, req_body) = req.into_parts();
    let req_body_bytes = collect_capped(req_body, max_body).await?;

    let captured_request =
        build_captured_request(&req_parts, &req_body_bytes, &ctx, &state, started_us);

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
    let is_sse = resp_parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        Ok(forward_streaming_sse(
            resp_parts,
            resp_body,
            captured_request,
            ctx,
            state,
            first_byte_us,
            max_body,
        ))
    } else {
        forward_buffered(
            resp_parts,
            resp_body,
            captured_request,
            ctx,
            state,
            first_byte_us,
            max_body,
        )
        .await
    }
}

/// Non-streaming path: fully buffer the upstream response, submit the
/// capture, then return a `Full<Bytes>`-bodied response to the client.
/// Matches the behavior of the original v1 forwarder.
async fn forward_buffered(
    resp_parts: http::response::Parts,
    resp_body: Incoming,
    captured_request: ts_protocol::model::HttpRequestData,
    ctx: TunnelContext,
    state: Arc<ProxyState>,
    first_byte_us: i64,
    max_body: usize,
) -> Result<Response<ResponseBody>, ForwardError> {
    let resp_body_bytes = collect_capped(resp_body, max_body).await?;
    let complete_us = now_us();

    let captured_response = build_captured_response(
        &resp_parts,
        resp_body_bytes.clone(),
        &captured_request,
        first_byte_us,
        complete_us,
    );

    if let Some(tx) = state.deps.joiner_event_tx.as_ref() {
        submit_exchange(tx, captured_request, captured_response, Vec::new());
    } else {
        debug!(target: "ts_proxy::forward", host = %ctx.host, "no joiner sink configured; capture dropped");
    }

    Ok(rebuild_client_response(
        resp_parts,
        box_full(Full::new(resp_body_bytes)),
    ))
}

/// Streaming path: hand a `CapturingBody`-wrapped upstream stream
/// straight back to the client. Each upstream frame mirrors into a
/// capture buffer; when the stream ends (or the client disconnects),
/// the finalizer builds the captured exchange + parses SSE events
/// from the accumulated bytes and submits it to the joiner channel.
fn forward_streaming_sse(
    resp_parts: http::response::Parts,
    resp_body: Incoming,
    captured_request: ts_protocol::model::HttpRequestData,
    ctx: TunnelContext,
    state: Arc<ProxyState>,
    first_byte_us: i64,
    max_body: usize,
) -> Response<ResponseBody> {
    // Snapshot everything the finalize closure needs as owned values
    // — `Parts` itself isn't Clone, and the closure outlives this fn.
    let status = resp_parts.status.as_u16();
    let version_minor = match resp_parts.version {
        http::Version::HTTP_10 => 0,
        _ => 1,
    };
    let resp_headers_for_capture: Vec<(String, String)> = resp_parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let client_addr = client_socket(&ctx);
    let upstream_addr = upstream_socket(&ctx);
    let flow_key = captured_request.flow_key.clone();
    let joiner_tx = state.deps.joiner_event_tx.clone();
    let host_for_log = ctx.host.clone();

    // The finalizer fires once per stream — either from the EOF arm of
    // `CapturingBody::poll_frame` (clean end), or its `Drop` impl
    // (client disconnected mid-stream). Either way: build the captured
    // response (body is empty per joiner SSE contract), parse the SSE
    // event stream from the accumulator, submit to the pipeline.
    let finalize: crate::body::Finalizer = Box::new(move |body_bytes: Bytes| {
        let complete_us = now_us();
        let captured_response = make_response_data(
            flow_key.clone(),
            client_addr,
            upstream_addr,
            status,
            version_minor,
            resp_headers_for_capture.clone(),
            Bytes::new(),
            first_byte_us,
            complete_us,
        );
        let raw = std::str::from_utf8(&body_bytes).unwrap_or("");
        let sse_events =
            parse_sse_chunk(&flow_key, client_addr, upstream_addr, raw, first_byte_us);
        if let Some(tx) = joiner_tx.as_ref() {
            submit_exchange(tx, captured_request.clone(), captured_response, sse_events);
        } else {
            debug!(
                target: "ts_proxy::forward",
                host = %host_for_log,
                "no joiner sink configured; SSE capture dropped"
            );
        }
    });

    let capturing = CapturingBody::new(resp_body, max_body, finalize);
    rebuild_client_response(resp_parts, capturing.boxed_unsync())
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

/// Forward the upstream response back to the client. Strips hop-by-hop
/// response headers per RFC 7230 §6.1 — most matter only for chunked
/// transport, but stripping them keeps strict clients (e.g. nodejs http
/// parsers) happy when the body is re-framed.
fn rebuild_client_response(
    resp_parts: http::response::Parts,
    body: ResponseBody,
) -> Response<ResponseBody> {
    let strip: &[HeaderName] = &[
        http::header::CONNECTION,
        http::header::TRANSFER_ENCODING,
        http::header::TRAILER,
        http::header::UPGRADE,
        http::header::CONTENT_LENGTH,
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
        .body(body)
        .unwrap_or_else(|_| make_static_502_response())
}

fn make_static_502_response() -> Response<ResponseBody> {
    Response::builder()
        .status(502)
        .body(box_full(Full::new(Bytes::from_static(
            b"tokenscope: response rebuild failed",
        ))))
        .expect("static")
}

/// `Full<Bytes>` has `Error = Infallible`. The proxy response type
/// uses `hyper::Error` so streaming and buffered paths share one body
/// type. Map the never-type → never to satisfy the converter; the
/// `match` arm is uninhabited so the compiler proves no runtime cost.
pub(crate) fn box_full(body: Full<Bytes>) -> ResponseBody {
    body.map_err(|never: std::convert::Infallible| match never {})
        .boxed_unsync()
}

fn client_socket(ctx: &TunnelContext) -> SocketAddr {
    ctx.client_peer
}

fn upstream_socket(ctx: &TunnelContext) -> SocketAddr {
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
    fn into_response(self) -> Response<ResponseBody> {
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

fn json_response(status: u16, body: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("server", "tokenscope-proxy/0.2")
        .body(box_full(Full::new(Bytes::copy_from_slice(body.as_bytes()))))
        .expect("static response")
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
