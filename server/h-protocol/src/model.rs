use bytes::Bytes;
use h_common::process::ProcessInfo;

use crate::net::FlowKey;

/// Events emitted by h-protocol for consumption by h-llm.
#[derive(Debug, Clone)]
pub enum HttpParseEvent {
    /// A complete HTTP request (headers + body) has been parsed.
    HttpRequest(HttpRequestData),
    /// A complete HTTP response (headers + body) has been parsed.
    /// For SSE responses, body contains the raw concatenated SSE text.
    HttpResponse(HttpResponseData),
    /// An individual SSE event from a streaming response.
    SseEvent(SseEventData),
    /// A time-advancing heartbeat. Carries `wall_ts_us` (Unix-epoch µs).
    /// Emitted by each shard when the upstream dispatcher broadcasts a
    /// heartbeat. Downstream stages that are driven by packet timestamps
    /// (turn sweep, metrics window close) use these to make progress during
    /// idle traffic without needing a separate wall-clock ticker.
    Heartbeat { ts: i64, source_id: String },
}

/// A single Server-Sent Event parsed from a `text/event-stream` response.
#[derive(Debug, Clone)]
pub struct SseEventData {
    pub flow_key: FlowKey,
    pub client_addr: (std::net::IpAddr, u16),
    pub server_addr: (std::net::IpAddr, u16),
    /// SSE `event:` field (e.g., "content_block_delta"). Empty if not specified.
    pub event_type: String,
    /// SSE `data:` field content.
    pub data: String,
    pub timestamp_us: i64,
    /// Owning process, stamped by the flow when the source attributes it (eBPF).
    pub process: Option<ProcessInfo>,
}

/// A fully parsed HTTP request.
#[derive(Debug, Clone)]
pub struct HttpRequestData {
    pub flow_key: FlowKey,
    /// Original source (client) IP and port.
    pub client_addr: (std::net::IpAddr, u16),
    /// Original destination (server) IP and port.
    pub server_addr: (std::net::IpAddr, u16),
    pub method: String,
    pub uri: String,
    pub version: u8, // 0 = HTTP/1.0, 1 = HTTP/1.1
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub timestamp_us: i64,
    /// Owning process, stamped by the flow when the source attributes it (eBPF).
    pub process: Option<ProcessInfo>,
}

/// A fully parsed HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponseData {
    pub flow_key: FlowKey,
    /// Original source (client) IP and port (same as the request it answers).
    pub client_addr: (std::net::IpAddr, u16),
    /// Original destination (server) IP and port.
    pub server_addr: (std::net::IpAddr, u16),
    pub status: u16,
    pub version: u8,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// Timestamp of the first response byte (for TTFT calculation).
    pub first_byte_timestamp_us: i64,
    /// Timestamp when the response was fully received (for E2E latency).
    pub complete_timestamp_us: i64,
    /// Owning process, stamped by the flow when the source attributes it (eBPF).
    pub process: Option<ProcessInfo>,
}

impl HttpRequestData {
    /// Find a header value by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Get Content-Type header.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }
}

impl HttpResponseData {
    /// Find a header value by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Get Content-Type header.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }
}

impl HttpParseEvent {
    /// Stamp process attribution onto a content event (request / response / SSE).
    /// A no-op for [`HttpParseEvent::Heartbeat`], which carries no process. The
    /// flow calls this on every event it emits, once it has learned the owning
    /// process from the connection's first attributed packet.
    pub fn set_process(&mut self, process: Option<ProcessInfo>) {
        match self {
            HttpParseEvent::HttpRequest(r) => r.process = process,
            HttpParseEvent::HttpResponse(r) => r.process = process,
            HttpParseEvent::SseEvent(s) => s.process = process,
            HttpParseEvent::Heartbeat { .. } => {}
        }
    }
}

impl std::fmt::Display for HttpParseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpParseEvent::HttpRequest(req) => {
                write!(
                    f,
                    "[REQ]  {}:{} -> {}:{} | {} {} | {}B",
                    req.client_addr.0,
                    req.client_addr.1,
                    req.server_addr.0,
                    req.server_addr.1,
                    req.method,
                    req.uri,
                    req.body.len(),
                )
            }
            HttpParseEvent::HttpResponse(resp) => {
                let ct = resp.content_type().unwrap_or("-");
                write!(
                    f,
                    "[RESP] {}:{} -> {}:{} | {} | {}B | {ct}",
                    resp.client_addr.0,
                    resp.client_addr.1,
                    resp.server_addr.0,
                    resp.server_addr.1,
                    resp.status,
                    resp.body.len(),
                )
            }
            HttpParseEvent::SseEvent(sse) => {
                let data_preview: String = sse.data.chars().take(80).collect();
                write!(
                    f,
                    "[SSE]  {}:{} -> {}:{} | {} | {}",
                    sse.client_addr.0,
                    sse.client_addr.1,
                    sse.server_addr.0,
                    sse.server_addr.1,
                    sse.event_type,
                    data_preview,
                )
            }
            HttpParseEvent::Heartbeat { ts, source_id } => {
                write!(f, "[HB]   wall_ts_us={ts} source={source_id}")
            }
        }
    }
}
