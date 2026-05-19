//! Construct `HttpJoinerEvent::Exchange` records from observed
//! request/response bytes and submit them to the storage pipeline.
//!
//! The proxy doesn't have real TCP-level flow keys (everything is on
//! one upgraded socket — peer addressing reflects the proxy, not the
//! real upstream). We synthesize a `FlowKey` that's stable per CONNECT
//! and per session: the client's TCP peer for `addr_a`, the resolved
//! upstream `host:port` for `addr_b`, both stamped with the configured
//! `source_id` so the UI source filter can pick out proxy-originated
//! rows.

use bytes::Bytes;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use ts_protocol::joiner::HttpJoinerEvent;
use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};
use ts_protocol::net::FlowKey;
use uuid::Uuid;

/// Resolve a hostname-like authority into an IP for FlowKey
/// purposes. We only ever use this for storage display, never to
/// actually dial anything — so a parsed-or-loopback fallback is fine.
fn host_to_ip(host: &str) -> IpAddr {
    host.parse().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Build the `FlowKey` that stamps captured records. `source_id` is the
/// ProxyConfig source label (default `"builtin-proxy"`); `client_peer`
/// is the upstream TCP peer of the *proxy's* listener (i.e. whoever
/// dialled us); `host` is the CONNECT authority (`api.openai.com`).
pub fn make_flow_key(source_id: &str, client_peer: SocketAddr, host: &str, port: u16) -> FlowKey {
    FlowKey::new(
        source_id.to_string(),
        client_peer.ip(),
        client_peer.port(),
        host_to_ip(host),
        port,
    )
}

/// Build the `HttpRequestData` we hand to the storage pipeline. The
/// `headers` vec must already have any policy-driven redaction applied
/// by the caller — `capture.rs` makes no edits.
#[allow(clippy::too_many_arguments)]
pub fn make_request_data(
    flow_key: FlowKey,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    method: String,
    uri: String,
    version_minor: u8,
    headers: Vec<(String, String)>,
    body: Bytes,
    timestamp_us: i64,
) -> HttpRequestData {
    HttpRequestData {
        flow_key,
        client_addr: (client_addr.ip(), client_addr.port()),
        server_addr: (server_addr.ip(), server_addr.port()),
        method,
        uri,
        version: version_minor,
        headers,
        body,
        timestamp_us,
    }
}

/// Build the `HttpResponseData`. For SSE responses pass `body =
/// Bytes::new()` and provide the full SSE event list to
/// `submit_exchange`; the joiner contract is that SSE bodies are not
/// persisted (event stream is the canonical record).
#[allow(clippy::too_many_arguments)]
pub fn make_response_data(
    flow_key: FlowKey,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    status: u16,
    version_minor: u8,
    headers: Vec<(String, String)>,
    body: Bytes,
    first_byte_timestamp_us: i64,
    complete_timestamp_us: i64,
) -> HttpResponseData {
    HttpResponseData {
        flow_key,
        client_addr: (client_addr.ip(), client_addr.port()),
        server_addr: (server_addr.ip(), server_addr.port()),
        status,
        version: version_minor,
        headers,
        body,
        first_byte_timestamp_us,
        complete_timestamp_us,
    }
}

/// Parse a chunk of raw SSE wire bytes into individual events. This is
/// a minimal SSE framer: split on blank lines, then within each event
/// stitch `data:` lines together and pull out `event:` when present.
/// Comment lines (`:`) and arbitrary unknown fields are ignored, which
/// matches what the existing sniffer-side parser does.
pub fn parse_sse_chunk(
    flow_key: &FlowKey,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    raw: &str,
    base_timestamp_us: i64,
) -> Vec<SseEventData> {
    let mut events = Vec::new();
    for frame in raw.split("\n\n") {
        let frame = frame.trim_end_matches('\r');
        if frame.is_empty() {
            continue;
        }
        let mut event_type = String::new();
        let mut data = String::new();
        for line in frame.split('\n') {
            let line = line.trim_end_matches('\r');
            if line.starts_with(':') || line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim_start_matches(' ').to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start_matches(' '));
            }
            // Unknown fields (id:, retry:, etc.) ignored — they don't
            // carry semantic content for any wire-api we support.
        }
        if !data.is_empty() || !event_type.is_empty() {
            events.push(SseEventData {
                flow_key: flow_key.clone(),
                client_addr: (client_addr.ip(), client_addr.port()),
                server_addr: (server_addr.ip(), server_addr.port()),
                event_type,
                data,
                timestamp_us: base_timestamp_us,
            });
        }
    }
    events
}

/// Submit one captured exchange to the storage pipeline. Best-effort:
/// if the joiner channel is closed or full we drop the event and log,
/// rather than backpressure the client response (we've already sent
/// the response bytes by this point in the proxy's forward path).
///
/// Synchronous on purpose — `try_send` is non-blocking, and the
/// streaming-body finalizer needs to call this from `Drop` / sync
/// `poll_frame` paths where `.await` isn't available.
pub fn submit_exchange(
    tx: &tokio::sync::mpsc::Sender<HttpJoinerEvent>,
    request: HttpRequestData,
    response: HttpResponseData,
    sse_events: Vec<SseEventData>,
) {
    let id = Uuid::now_v7().to_string();
    let event = HttpJoinerEvent::Exchange {
        id,
        request: Arc::new(request),
        response: Arc::new(response),
        sse_events,
    };
    if tx.try_send(event).is_err() {
        // try_send keeps us from blocking the response path on a
        // saturated sink. Storage backpressure on a high-throughput
        // proxy would otherwise stall HTTP responses to the LLM client.
        tracing::warn!(target: "ts_proxy::capture", "joiner channel full or closed; capture dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }

    #[test]
    fn flow_key_normalizes_consistently() {
        let fk1 = make_flow_key("builtin-proxy", sa("127.0.0.1", 50000), "api.openai.com", 443);
        let fk2 = make_flow_key("builtin-proxy", sa("127.0.0.1", 50000), "api.openai.com", 443);
        assert_eq!(fk1, fk2);
    }

    #[test]
    fn parse_sse_basic_event() {
        let fk = make_flow_key("p", sa("1.1.1.1", 100), "api.x", 443);
        let raw = "event: message_start\ndata: {\"hello\":\"world\"}\n\n";
        let events = parse_sse_chunk(&fk, sa("1.1.1.1", 100), sa("2.2.2.2", 443), raw, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "message_start");
        assert_eq!(events[0].data, "{\"hello\":\"world\"}");
    }

    #[test]
    fn parse_sse_multiline_data_concatenated() {
        let fk = make_flow_key("p", sa("1.1.1.1", 100), "api.x", 443);
        let raw = "data: line one\ndata: line two\n\n";
        let events = parse_sse_chunk(&fk, sa("1.1.1.1", 100), sa("2.2.2.2", 443), raw, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line one\nline two");
    }

    #[test]
    fn parse_sse_multiple_events() {
        let fk = make_flow_key("p", sa("1.1.1.1", 100), "api.x", 443);
        let raw = "data: a\n\ndata: b\n\ndata: c\n\n";
        let events = parse_sse_chunk(&fk, sa("1.1.1.1", 100), sa("2.2.2.2", 443), raw, 0);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "a");
        assert_eq!(events[2].data, "c");
    }

    #[test]
    fn parse_sse_ignores_comments() {
        let fk = make_flow_key("p", sa("1.1.1.1", 100), "api.x", 443);
        let raw = ": keepalive\n\ndata: actual\n\n";
        let events = parse_sse_chunk(&fk, sa("1.1.1.1", 100), sa("2.2.2.2", 443), raw, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "actual");
    }

    #[test]
    fn parse_sse_handles_crlf() {
        let fk = make_flow_key("p", sa("1.1.1.1", 100), "api.x", 443);
        let raw = "event: foo\r\ndata: bar\r\n\r\n";
        let events = parse_sse_chunk(&fk, sa("1.1.1.1", 100), sa("2.2.2.2", 443), raw, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "foo");
        assert_eq!(events[0].data, "bar");
    }
}
