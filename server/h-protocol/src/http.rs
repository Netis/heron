use std::io::Write;
use std::net::IpAddr;

use bytes::{Bytes, BytesMut};
use flate2::write::{GzDecoder, ZlibDecoder};

use crate::model::{HttpParseEvent, HttpRequestData, HttpResponseData, SseEventData};
use crate::net::FlowKey;

/// State of the HTTP parser for one TCP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    /// Waiting for a request from the client side.
    WaitingForRequest,
    /// Request headers parsed, reading request body.
    ReadingRequestBody,
    /// Request complete, waiting for response headers.
    WaitingForResponse,
    /// Response headers parsed, reading response body.
    ReadingResponseBody,
}

/// How the message body is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFraming {
    /// No body (1xx, 204, 304, HEAD response, or request without body indicators).
    NoBody,
    /// Content-Length header present.
    ContentLength(usize),
    /// Transfer-Encoding: chunked.
    Chunked,
    /// No framing info — body ends when the connection closes.
    CloseDelimited,
}

/// Result of a single `BodyReader::read()` call.
#[derive(Debug)]
enum ReadResult {
    /// Not enough data yet.
    NeedMore,
    /// Body is complete. Contains the full decoded body.
    Complete(Bytes),
    /// One chunk of decoded body data (chunked or close-delimited).
    /// The full body is accumulated internally and returned by `Complete` or `finish()`.
    ChunkDecoded(Bytes),
    /// Unrecoverable decode error (e.g. invalid chunk size).
    Error,
}

/// Streaming `Content-Encoding` decompressor for an HTTP body.
///
/// Compressed bytes are pushed in as they arrive (one de-chunked chunk at a
/// time for SSE, or the whole body for Content-Length) and decompressed bytes
/// come out incrementally, so the SSE parser keeps seeing plaintext. The real
/// motivator: api.anthropic.com gzip-compresses its `text/event-stream`
/// responses, and without this every Anthropic call lands with an empty
/// response body (no tokens / tool_use / model). gzip + deflate are handled via
/// flate2 (pure-Rust miniz_oxide); other encodings (`br`, …) are treated as
/// unsupported and left to the caller as passthrough.
///
/// Best-effort: a malformed stream yields whatever decompressed so far rather
/// than failing the parse — this is observability data, not a correctness gate.
enum BodyDecompressor {
    Gzip(GzDecoder<Vec<u8>>),
    /// `Content-Encoding: deflate` per RFC 7230 is the zlib format (RFC 1950).
    Zlib(ZlibDecoder<Vec<u8>>),
}

impl BodyDecompressor {
    /// Build from a `Content-Encoding` value, or `None` for identity / an
    /// encoding we don't decode (caller then treats the body as plaintext).
    fn from_encoding(enc: &str) -> Option<Self> {
        match enc.trim().to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => Some(Self::Gzip(GzDecoder::new(Vec::new()))),
            "deflate" => Some(Self::Zlib(ZlibDecoder::new(Vec::new()))),
            _ => None,
        }
    }

    fn writer(&mut self) -> &mut dyn Write {
        match self {
            Self::Gzip(d) => d,
            Self::Zlib(d) => d,
        }
    }

    fn drain(&mut self) -> Vec<u8> {
        match self {
            Self::Gzip(d) => std::mem::take(d.get_mut()),
            Self::Zlib(d) => std::mem::take(d.get_mut()),
        }
    }

    /// Push compressed bytes; return whatever decompressed output is ready.
    fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let _ = self.writer().write_all(data);
        let _ = self.writer().flush();
        self.drain()
    }

    /// Finish the stream, returning any trailing decompressed bytes.
    fn finish(self) -> Vec<u8> {
        match self {
            Self::Gzip(d) => d.finish().unwrap_or_default(),
            Self::Zlib(d) => d.finish().unwrap_or_default(),
        }
    }
}

/// Reads an HTTP message body according to its framing.
struct BodyReader {
    framing: BodyFraming,
    /// Accumulated decoded body (used by Chunked and CloseDelimited).
    decoded_body: BytesMut,
    /// When true, the reader still emits each `ChunkDecoded` so callers (the
    /// SSE parser) can consume chunks incrementally, but stops accumulating
    /// them into `decoded_body` and returns empty bytes from `Complete` and
    /// `finish`. Used for SSE responses whose raw body is never read by any
    /// downstream stage (LlmProcessor rebuilds response_body from SSE events).
    skip_accumulation: bool,
    /// `Content-Encoding` decompressor. `None` for the common uncompressed case,
    /// where the body passes through byte-for-byte unchanged.
    decompressor: Option<BodyDecompressor>,
}

impl BodyReader {
    fn new() -> Self {
        Self {
            framing: BodyFraming::NoBody,
            decoded_body: BytesMut::new(),
            skip_accumulation: false,
            decompressor: None,
        }
    }

    fn new_for_request(headers: &[(String, String)]) -> Self {
        let framing = if is_chunked(headers) {
            BodyFraming::Chunked
        } else if let Some(len) = extract_content_length(headers) {
            BodyFraming::ContentLength(len)
        } else {
            BodyFraming::NoBody
        };
        Self {
            framing,
            decoded_body: BytesMut::new(),
            skip_accumulation: false,
            // Request bodies are not decompressed (agent clients send plaintext
            // JSON); only responses carry Content-Encoding in practice.
            decompressor: None,
        }
    }

    fn new_for_response(status: u16, req_method: &str, headers: &[(String, String)]) -> Self {
        let framing = if (100..200).contains(&status) || status == 204 || status == 304 {
            BodyFraming::NoBody
        } else if req_method.eq_ignore_ascii_case("HEAD") {
            BodyFraming::NoBody
        } else if is_chunked(headers) {
            BodyFraming::Chunked
        } else if let Some(len) = extract_content_length(headers) {
            BodyFraming::ContentLength(len)
        } else {
            BodyFraming::CloseDelimited
        };
        Self {
            framing,
            decoded_body: BytesMut::new(),
            skip_accumulation: false,
            decompressor: extract_content_encoding(headers)
                .as_deref()
                .and_then(BodyDecompressor::from_encoding),
        }
    }

    /// Pass de-chunked body bytes through the `Content-Encoding` decompressor,
    /// if any. With no decompressor (the common case) this is an identity copy,
    /// so uncompressed traffic is byte-for-byte unchanged.
    fn decode_payload(&mut self, data: &[u8]) -> Bytes {
        match self.decompressor.as_mut() {
            Some(d) => Bytes::from(d.push(data)),
            None => Bytes::copy_from_slice(data),
        }
    }

    /// Flush trailing decompressed bytes at end-of-body (terminal chunk / close).
    /// Empty when there is no decompressor or nothing is buffered.
    fn flush_decompressor(&mut self) -> Bytes {
        match self.decompressor.take() {
            Some(d) => Bytes::from(d.finish()),
            None => Bytes::new(),
        }
    }

    fn set_skip_accumulation(&mut self, skip: bool) {
        self.skip_accumulation = skip;
    }

    fn read(&mut self, buf: &mut BytesMut) -> ReadResult {
        match self.framing {
            BodyFraming::NoBody => ReadResult::Complete(Bytes::new()),
            BodyFraming::ContentLength(len) => {
                if buf.len() >= len {
                    let raw = buf.split_to(len);
                    let body = self.decode_payload(&raw);
                    let tail = self.flush_decompressor();
                    let body = if tail.is_empty() {
                        body
                    } else {
                        let mut v = body.to_vec();
                        v.extend_from_slice(&tail);
                        Bytes::from(v)
                    };
                    ReadResult::Complete(body)
                } else {
                    ReadResult::NeedMore
                }
            }
            BodyFraming::Chunked => self.read_chunk(buf),
            BodyFraming::CloseDelimited => {
                if buf.is_empty() {
                    ReadResult::NeedMore
                } else {
                    let raw = buf.split();
                    let data = self.decode_payload(&raw);
                    if !self.skip_accumulation {
                        self.decoded_body.extend_from_slice(&data);
                    }
                    ReadResult::ChunkDecoded(data)
                }
            }
        }
    }

    fn read_chunk(&mut self, buf: &mut BytesMut) -> ReadResult {
        if buf.is_empty() {
            return ReadResult::NeedMore;
        }

        let line_end = match find_crlf(buf) {
            Some(pos) => pos,
            None => return ReadResult::NeedMore,
        };

        let size_str = std::str::from_utf8(&buf[..line_end]).unwrap_or("").trim();
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let chunk_size = match usize::from_str_radix(size_str, 16) {
            Ok(s) => s,
            Err(_) => {
                return ReadResult::Error;
            }
        };

        if chunk_size == 0 {
            // Terminal chunk. Verify we can consume the trailer section before committing.
            let rest = &buf[line_end + 2..];
            let mut pos = 0;
            loop {
                match find_crlf(&rest[pos..]) {
                    Some(len) => {
                        if len == 0 {
                            // Empty line — end of trailers.
                            let total = line_end + 2 + pos + 2;
                            let _ = buf.split_to(total);
                            // Flush any decompressor remainder into the body.
                            let tail = self.flush_decompressor();
                            if !self.skip_accumulation && !tail.is_empty() {
                                self.decoded_body.extend_from_slice(&tail);
                            }
                            let body = if self.skip_accumulation {
                                Bytes::new()
                            } else {
                                self.decoded_body.split().freeze()
                            };
                            return ReadResult::Complete(body);
                        }
                        pos += len + 2;
                    }
                    None => return ReadResult::NeedMore,
                }
            }
        }

        // Data chunk: need size_line + chunk_data + trailing \r\n.
        let needed = line_end + 2 + chunk_size + 2;
        if buf.len() < needed {
            return ReadResult::NeedMore;
        }

        let _ = buf.split_to(line_end + 2);
        let raw = buf.split_to(chunk_size);
        let _ = buf.split_to(2);
        let chunk_data = self.decode_payload(&raw);
        if !self.skip_accumulation {
            self.decoded_body.extend_from_slice(&chunk_data);
        }
        ReadResult::ChunkDecoded(chunk_data)
    }

    /// Flush remaining data as body on connection close.
    fn finish(&mut self, buf: &mut BytesMut) -> Bytes {
        match self.framing {
            BodyFraming::NoBody => Bytes::new(),
            BodyFraming::ContentLength(_) => {
                let raw = buf.split();
                if self.skip_accumulation {
                    Bytes::new()
                } else {
                    let mut body = self.decode_payload(&raw).to_vec();
                    body.extend_from_slice(&self.flush_decompressor());
                    Bytes::from(body)
                }
            }
            BodyFraming::Chunked | BodyFraming::CloseDelimited => {
                if self.skip_accumulation {
                    buf.clear();
                    Bytes::new()
                } else {
                    if !buf.is_empty() {
                        let raw = buf.split();
                        let data = self.decode_payload(&raw);
                        self.decoded_body.extend_from_slice(&data);
                    }
                    let tail = self.flush_decompressor();
                    if !tail.is_empty() {
                        self.decoded_body.extend_from_slice(&tail);
                    }
                    self.decoded_body.split().freeze()
                }
            }
        }
    }

    /// Whether body data was fed incrementally via `ChunkDecoded`.
    fn was_incremental(&self) -> bool {
        matches!(
            self.framing,
            BodyFraming::Chunked | BodyFraming::CloseDelimited
        )
    }

    fn is_no_body(&self) -> bool {
        self.framing == BodyFraming::NoBody
    }
}

/// Parses SSE events from a stream of text data.
struct SseParser {
    residual: String,
}

impl SseParser {
    fn new() -> Self {
        Self {
            residual: String::new(),
        }
    }

    fn reset(&mut self) {
        self.residual.clear();
    }

    /// Feed a chunk of text. Complete SSE events are emitted to `output`.
    fn push(
        &mut self,
        text: &str,
        flow_key: &FlowKey,
        client_addr: (IpAddr, u16),
        server_addr: (IpAddr, u16),
        timestamp: i64,
        output: &mut Vec<HttpParseEvent>,
    ) {
        self.residual.push_str(text);

        loop {
            // SSE events are separated by blank lines.
            let (sep_pos, skip) = if let Some(pos) = self.residual.find("\r\n\r\n") {
                (pos, 4)
            } else if let Some(pos) = self.residual.find("\n\n") {
                (pos, 2)
            } else {
                break;
            };

            let event_text = self.residual[..sep_pos].to_string();
            self.residual = self.residual[sep_pos + skip..].to_string();

            if let Some(evt) =
                Self::parse_event(&event_text, flow_key, client_addr, server_addr, timestamp)
            {
                output.push(evt);
            }
        }
    }

    /// Flush any remaining residual as a final event.
    fn flush(
        &mut self,
        flow_key: &FlowKey,
        client_addr: (IpAddr, u16),
        server_addr: (IpAddr, u16),
        timestamp: i64,
        output: &mut Vec<HttpParseEvent>,
    ) {
        let residual = std::mem::take(&mut self.residual);
        let trimmed = residual.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(evt) = Self::parse_event(trimmed, flow_key, client_addr, server_addr, timestamp)
        {
            output.push(evt);
        }
    }

    fn parse_event(
        text: &str,
        flow_key: &FlowKey,
        client_addr: (IpAddr, u16),
        server_addr: (IpAddr, u16),
        timestamp: i64,
    ) -> Option<HttpParseEvent> {
        let mut event_type = String::new();
        let mut data_parts: Vec<&str> = Vec::new();

        for line in text.lines() {
            if let Some(val) = line.strip_prefix("event:") {
                event_type = val.trim().to_string();
            } else if let Some(val) = line.strip_prefix("data:") {
                data_parts.push(val.trim_start_matches(' '));
            } else if line.starts_with(':') {
                // Comment, skip.
            }
        }

        if data_parts.is_empty() && event_type.is_empty() {
            return None;
        }

        Some(HttpParseEvent::SseEvent(SseEventData {
            flow_key: flow_key.clone(),
            client_addr,
            server_addr,
            event_type,
            data: data_parts.join("\n"),
            timestamp_us: timestamp,
            // The flow stamps process attribution onto emitted events.
            process: None,
        }))
    }
}

/// Result of an `HttpParser::parse()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseResult {
    /// Normal: parsed what was available, waiting for more data.
    Ok,
    /// Unrecoverable error in current req-resp cycle. Caller should resync.
    NeedResync,
}

/// Incrementally parses HTTP request/response pairs from reassembled TCP buffers.
pub struct HttpParser {
    state: ParserState,

    // Pending request data (set after request headers are parsed).
    pending_method: String,
    pending_uri: String,
    pending_req_version: u8,
    pending_req_headers: Vec<(String, String)>,
    pending_req_timestamp: i64,
    // Kept across request/response cycle for response framing detection.
    last_request_method: String,

    // Pending response data.
    pending_resp_status: u16,
    pending_resp_version: u8,
    pending_resp_headers: Vec<(String, String)>,
    pending_resp_timestamp: i64,
    pending_resp_is_sse: bool,

    // Delegate components.
    body_reader: BodyReader,
    sse_parser: SseParser,
}

impl HttpParser {
    pub fn new() -> Self {
        Self {
            state: ParserState::WaitingForRequest,
            pending_method: String::new(),
            pending_uri: String::new(),
            pending_req_version: 1,
            pending_req_headers: Vec::new(),
            pending_req_timestamp: 0,
            last_request_method: String::new(),
            pending_resp_status: 0,
            pending_resp_version: 1,
            pending_resp_headers: Vec::new(),
            pending_resp_timestamp: 0,
            pending_resp_is_sse: false,
            body_reader: BodyReader::new(),
            sse_parser: SseParser::new(),
        }
    }

    /// Returns true when the parser is waiting for or reading a response.
    pub fn is_waiting_for_response(&self) -> bool {
        matches!(
            self.state,
            ParserState::WaitingForResponse | ParserState::ReadingResponseBody
        )
    }

    /// Reset the parser to its initial state, discarding any in-progress parsing.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Try to parse HTTP messages from the client and server buffers.
    /// Parsed bytes are drained from the buffers. Events are pushed to `output`.
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        &mut self,
        client_buf: &mut BytesMut,
        server_buf: &mut BytesMut,
        flow_key: &FlowKey,
        client_addr: (IpAddr, u16),
        server_addr: (IpAddr, u16),
        client_ts: i64,
        server_ts: i64,
        server_last_ts: i64,
        output: &mut Vec<HttpParseEvent>,
    ) -> ParseResult {
        'outer: loop {
            match self.state {
                ParserState::WaitingForRequest => {
                    match self.try_parse_request_headers(client_buf, client_ts) {
                        Some(true) => {}      // headers parsed, continue
                        Some(false) => break, // need more data
                        None => return ParseResult::NeedResync,
                    }
                    self.body_reader = BodyReader::new_for_request(&self.pending_req_headers);
                    self.state = ParserState::ReadingRequestBody;
                }
                ParserState::ReadingRequestBody => loop {
                    match self.body_reader.read(client_buf) {
                        ReadResult::ChunkDecoded(_) => continue,
                        ReadResult::Complete(body) => {
                            output.push(HttpParseEvent::HttpRequest(HttpRequestData {
                                flow_key: flow_key.clone(),
                                client_addr,
                                server_addr,
                                method: std::mem::take(&mut self.pending_method),
                                uri: std::mem::take(&mut self.pending_uri),
                                version: self.pending_req_version,
                                headers: std::mem::take(&mut self.pending_req_headers),
                                body,
                                timestamp_us: self.pending_req_timestamp,
                                process: None,
                            }));
                            self.state = ParserState::WaitingForResponse;
                            break;
                        }
                        ReadResult::NeedMore => break 'outer,
                        ReadResult::Error => return ParseResult::NeedResync,
                    }
                },
                ParserState::WaitingForResponse => {
                    match self.try_parse_response_headers(server_buf, server_ts) {
                        Some(true) => {}      // headers parsed, continue
                        Some(false) => break, // need more data
                        None => return ParseResult::NeedResync,
                    }
                    self.body_reader = BodyReader::new_for_response(
                        self.pending_resp_status,
                        &self.last_request_method,
                        &self.pending_resp_headers,
                    );
                    if self.pending_resp_is_sse {
                        // SSE body is consumed event-by-event via ChunkDecoded
                        // and never read back as raw bytes — skip accumulation
                        // to avoid holding the full stream in memory.
                        self.body_reader.set_skip_accumulation(true);
                        self.sse_parser.reset();
                    }
                    if self.body_reader.is_no_body() {
                        // No body — emit response immediately.
                        output.push(HttpParseEvent::HttpResponse(HttpResponseData {
                            flow_key: flow_key.clone(),
                            client_addr,
                            server_addr,
                            status: self.pending_resp_status,
                            version: self.pending_resp_version,
                            headers: std::mem::take(&mut self.pending_resp_headers),
                            body: Bytes::new(),
                            first_byte_timestamp_us: self.pending_resp_timestamp,
                            complete_timestamp_us: server_ts,
                            process: None,
                        }));
                        self.state = ParserState::WaitingForRequest;
                        continue;
                    }
                    self.state = ParserState::ReadingResponseBody;
                }
                ParserState::ReadingResponseBody => loop {
                    match self.body_reader.read(server_buf) {
                        ReadResult::ChunkDecoded(chunk) => {
                            if self.pending_resp_is_sse {
                                if let Ok(text) = std::str::from_utf8(&chunk) {
                                    self.sse_parser.push(
                                        text,
                                        flow_key,
                                        client_addr,
                                        server_addr,
                                        server_last_ts,
                                        output,
                                    );
                                }
                            }
                            continue;
                        }
                        ReadResult::Complete(body) => {
                            if self.pending_resp_is_sse {
                                if !self.body_reader.was_incremental() {
                                    if let Ok(text) = std::str::from_utf8(&body) {
                                        self.sse_parser.push(
                                            text,
                                            flow_key,
                                            client_addr,
                                            server_addr,
                                            server_last_ts,
                                            output,
                                        );
                                    }
                                }
                                self.sse_parser.flush(
                                    flow_key,
                                    client_addr,
                                    server_addr,
                                    server_last_ts,
                                    output,
                                );
                            }
                            // SSE responses do not retain a raw body: the
                            // event stream is the canonical form and the
                            // Content-Length SSE path uses `body` above only
                            // to feed the parser.
                            let emitted_body = if self.pending_resp_is_sse {
                                Bytes::new()
                            } else {
                                body
                            };
                            output.push(HttpParseEvent::HttpResponse(HttpResponseData {
                                flow_key: flow_key.clone(),
                                client_addr,
                                server_addr,
                                status: self.pending_resp_status,
                                version: self.pending_resp_version,
                                headers: std::mem::take(&mut self.pending_resp_headers),
                                body: emitted_body,
                                first_byte_timestamp_us: self.pending_resp_timestamp,
                                complete_timestamp_us: server_last_ts,
                                process: None,
                            }));
                            self.state = ParserState::WaitingForRequest;
                            break;
                        }
                        ReadResult::NeedMore => break 'outer,
                        ReadResult::Error => return ParseResult::NeedResync,
                    }
                },
            }
        }
        ParseResult::Ok
    }

    /// Force-finish an in-progress response (e.g. on connection close).
    pub fn finish_response(
        &mut self,
        server_buf: &mut BytesMut,
        flow_key: &FlowKey,
        client_addr: (IpAddr, u16),
        server_addr: (IpAddr, u16),
        server_last_ts: i64,
        output: &mut Vec<HttpParseEvent>,
    ) {
        if self.state != ParserState::ReadingResponseBody {
            return;
        }
        let body = self.body_reader.finish(server_buf);
        if self.pending_resp_is_sse {
            if !self.body_reader.was_incremental() {
                if let Ok(text) = std::str::from_utf8(&body) {
                    self.sse_parser.push(
                        text,
                        flow_key,
                        client_addr,
                        server_addr,
                        server_last_ts,
                        output,
                    );
                }
            }
            self.sse_parser
                .flush(flow_key, client_addr, server_addr, server_last_ts, output);
        }
        let emitted_body = if self.pending_resp_is_sse {
            Bytes::new()
        } else {
            body
        };
        output.push(HttpParseEvent::HttpResponse(HttpResponseData {
            flow_key: flow_key.clone(),
            client_addr,
            server_addr,
            status: self.pending_resp_status,
            version: self.pending_resp_version,
            headers: std::mem::take(&mut self.pending_resp_headers),
            body: emitted_body,
            first_byte_timestamp_us: self.pending_resp_timestamp,
            complete_timestamp_us: server_last_ts,
            process: None,
        }));
        self.state = ParserState::WaitingForRequest;
    }

    /// Try to parse HTTP request headers from the buffer.
    /// Returns `Some(true)` on success, `Some(false)` if more data is needed, `None` on error.
    fn try_parse_request_headers(&mut self, buf: &mut BytesMut, timestamp: i64) -> Option<bool> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);

        match req.parse(buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                self.pending_method = req.method.unwrap_or("").to_string();
                self.pending_uri = req.path.unwrap_or("").to_string();
                self.pending_req_version = req.version.unwrap_or(1);
                self.pending_req_headers = req
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_string(),
                            String::from_utf8_lossy(h.value).to_string(),
                        )
                    })
                    .collect();

                self.last_request_method = self.pending_method.clone();
                self.pending_req_timestamp = timestamp;

                let _ = buf.split_to(header_len);
                Some(true)
            }
            Ok(httparse::Status::Partial) => Some(false),
            Err(_) => None,
        }
    }

    /// Try to parse HTTP response headers from the buffer.
    /// Returns `Some(true)` on success, `Some(false)` if more data is needed, `None` on error.
    fn try_parse_response_headers(&mut self, buf: &mut BytesMut, timestamp: i64) -> Option<bool> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut resp = httparse::Response::new(&mut headers);

        match resp.parse(buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                self.pending_resp_status = resp.code.unwrap_or(0);
                self.pending_resp_version = resp.version.unwrap_or(1);
                self.pending_resp_headers = resp
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_string(),
                            String::from_utf8_lossy(h.value).to_string(),
                        )
                    })
                    .collect();

                // Detect SSE.
                self.pending_resp_is_sse = is_sse(&self.pending_resp_headers);
                self.pending_resp_timestamp = timestamp;

                let _ = buf.split_to(header_len);
                Some(true)
            }
            Ok(httparse::Status::Partial) => Some(false),
            Err(_) => None,
        }
    }
}

/// Find position of first \r\n in buffer.
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Check if Transfer-Encoding includes "chunked".
fn is_chunked(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_lowercase().contains("chunked")
    })
}

/// Check if Content-Type is text/event-stream.
fn is_sse(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type") && v.to_lowercase().contains("text/event-stream")
    })
}

/// Extract Content-Length from parsed headers.
fn extract_content_length(headers: &[(String, String)]) -> Option<usize> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Extract a decodable `Content-Encoding` from parsed headers, skipping
/// `identity` / empty. Returns the raw token (e.g. `"gzip"`) for
/// [`BodyDecompressor::from_encoding`].
fn extract_content_encoding(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("identity"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::FlowKey;

    fn test_flow() -> (FlowKey, (IpAddr, u16), (IpAddr, u16)) {
        let fk = FlowKey::new(
            String::new(),
            "127.0.0.1".parse().unwrap(),
            1000,
            "127.0.0.1".parse().unwrap(),
            8080,
        );
        let ca = ("127.0.0.1".parse().unwrap(), 1000);
        let sa = ("127.0.0.1".parse().unwrap(), 8080);
        (fk, ca, sa)
    }

    #[test]
    fn body_reader_gunzips_content_length_gzip() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let plain = b"{\"model\":\"claude-opus-4-6\",\"usage\":{\"output_tokens\":7}}";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(plain).unwrap();
        let gz = enc.finish().unwrap();
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Content-Encoding".to_string(), "gzip".to_string()),
            ("Content-Length".to_string(), gz.len().to_string()),
        ];
        let mut br = BodyReader::new_for_response(200, "POST", &headers);
        let mut buf = BytesMut::from(&gz[..]);
        match br.read(&mut buf) {
            ReadResult::Complete(body) => assert_eq!(&body[..], &plain[..]),
            _ => panic!("expected Complete with decompressed body"),
        }
    }

    #[test]
    fn body_reader_gunzips_chunked_gzip_sse() {
        // The real api.anthropic.com shape: text/event-stream + chunked +
        // Content-Encoding: gzip. Before the fix the SSE parser saw gzip bytes
        // and produced an empty body.
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let plain = b"event: message_start\r\ndata: {\"type\":\"message_start\"}\r\n\r\n";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(plain).unwrap();
        let gz = enc.finish().unwrap();
        // Wrap the gzip stream as one HTTP chunk + terminal chunk.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(format!("{:x}\r\n", gz.len()).as_bytes());
        buf.extend_from_slice(&gz);
        buf.extend_from_slice(b"\r\n0\r\n\r\n");
        let headers = vec![
            ("Content-Type".to_string(), "text/event-stream".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Content-Encoding".to_string(), "gzip".to_string()),
        ];
        let mut br = BodyReader::new_for_response(200, "POST", &headers);
        br.set_skip_accumulation(true); // SSE path: collect ChunkDecoded output
        let mut out = Vec::new();
        loop {
            match br.read(&mut buf) {
                ReadResult::ChunkDecoded(c) => out.extend_from_slice(&c),
                ReadResult::Complete(_) => break,
                ReadResult::NeedMore => panic!("unexpected NeedMore"),
                ReadResult::Error => panic!("decode error"),
            }
        }
        assert_eq!(out, plain);
    }

    #[test]
    fn test_parse_simple_request_response() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/chat/completions HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Length: 13\r\n\
             \r\n\
             {\"hello\":true}",
        );
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\n\
             Content-Length: 14\r\n\
             \r\n\
             {\"world\":true}",
        );

        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            1000000,
            2000000,
            2000000,
            &mut output,
        );

        assert_eq!(output.len(), 2);
        match &output[0] {
            HttpParseEvent::HttpRequest(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.uri, "/v1/chat/completions");
                assert_eq!(req.body.len(), 13);
            }
            _ => panic!("expected HttpRequest"),
        }
        match &output[1] {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body.len(), 14);
            }
            _ => panic!("expected HttpResponse"),
        }
    }

    #[test]
    fn test_chunked_response() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("GET /data HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\n\
             Transfer-Encoding: chunked\r\n\
             Content-Type: application/json\r\n\
             \r\n\
             5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );

        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        // Should get: HttpRequest + HttpResponse
        assert_eq!(output.len(), 2);
        match &output[1] {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(&resp.body[..], b"hello world");
            }
            _ => panic!("expected HttpResponse, got {:?}", output[1]),
        }
    }

    #[test]
    fn test_sse_chunked_response() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        );

        // Build chunked SSE response.
        let sse_body = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                         event: content_block_delta\ndata: {\"text\":\"Hello\"}\n\n\
                         event: message_stop\ndata: {}\n\n";
        let chunk = format!("{:x}\r\n{}\r\n0\r\n\r\n", sse_body.len(), sse_body);
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Transfer-Encoding: chunked\r\n\
             Content-Type: text/event-stream\r\n\
             \r\n\
             {chunk}"
        );
        let mut server_buf = BytesMut::from(resp.as_str());

        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        // Expect: HttpRequest + 3 SseEvents + HttpResponse
        let req_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
            .count();
        let sse_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::SseEvent(_)))
            .count();
        let resp_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
            .count();

        assert_eq!(req_count, 1);
        assert_eq!(sse_count, 3, "expected 3 SSE events, got {sse_count}");
        assert_eq!(resp_count, 1);

        // Check SSE event details.
        let sse_events: Vec<_> = output
            .iter()
            .filter_map(|e| match e {
                HttpParseEvent::SseEvent(s) => Some(s),
                _ => None,
            })
            .collect();

        assert_eq!(sse_events[0].event_type, "message_start");
        assert_eq!(sse_events[1].event_type, "content_block_delta");
        assert!(sse_events[1].data.contains("Hello"));
        assert_eq!(sse_events[2].event_type, "message_stop");

        // SSE responses no longer retain the raw body — the event stream is
        // the canonical form (already asserted above).
        match output.last().unwrap() {
            HttpParseEvent::HttpResponse(resp) => {
                assert!(resp.body.is_empty());
            }
            _ => panic!("expected HttpResponse last"),
        }
    }

    #[test]
    fn test_partial_request() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("GET /api HTTP/1.1\r\nHost: ex");
        let mut server_buf = BytesMut::new();
        let mut output = Vec::new();

        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );
        assert!(output.is_empty());

        client_buf.extend_from_slice(b"ample.com\r\n\r\n");
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        assert_eq!(output.len(), 1);
        match &output[0] {
            HttpParseEvent::HttpRequest(req) => {
                assert_eq!(req.method, "GET");
                assert_eq!(req.uri, "/api");
            }
            _ => panic!("expected HttpRequest"),
        }
    }

    #[test]
    fn test_extract_content_length() {
        let headers = vec![
            ("Host".to_string(), "localhost".to_string()),
            ("Content-Length".to_string(), "42".to_string()),
        ];
        assert_eq!(extract_content_length(&headers), Some(42));

        let no_cl = vec![("Host".to_string(), "localhost".to_string())];
        assert_eq!(extract_content_length(&no_cl), None);
    }

    #[test]
    fn test_is_chunked() {
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert!(is_chunked(&headers));

        let not_chunked = vec![("Content-Length".to_string(), "100".to_string())];
        assert!(!is_chunked(&not_chunked));
    }

    #[test]
    fn test_is_sse() {
        let headers = vec![("Content-Type".to_string(), "text/event-stream".to_string())];
        assert!(is_sse(&headers));

        let not_sse = vec![("Content-Type".to_string(), "application/json".to_string())];
        assert!(!is_sse(&not_sse));
    }

    // ── BodyReader unit tests ──

    #[test]
    fn test_body_reader_no_body() {
        let mut reader = BodyReader::new_for_response(204, "POST", &[]);
        let mut buf = BytesMut::from("leftover");
        match reader.read(&mut buf) {
            ReadResult::Complete(body) => assert!(body.is_empty()),
            other => panic!("expected Complete, got {other:?}"),
        }
        assert_eq!(&buf[..], b"leftover");
    }

    #[test]
    fn test_body_reader_no_body_1xx() {
        let mut reader = BodyReader::new_for_response(100, "GET", &[]);
        let mut buf = BytesMut::new();
        assert!(matches!(reader.read(&mut buf), ReadResult::Complete(_)));
    }

    #[test]
    fn test_body_reader_no_body_304() {
        let mut reader = BodyReader::new_for_response(304, "GET", &[]);
        let mut buf = BytesMut::new();
        assert!(matches!(reader.read(&mut buf), ReadResult::Complete(_)));
    }

    #[test]
    fn test_body_reader_no_body_head() {
        let headers = vec![("Content-Length".into(), "100".into())];
        let mut reader = BodyReader::new_for_response(200, "HEAD", &headers);
        let mut buf = BytesMut::new();
        assert!(matches!(reader.read(&mut buf), ReadResult::Complete(_)));
    }

    #[test]
    fn test_body_reader_content_length() {
        let headers = vec![("Content-Length".into(), "5".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        let mut buf = BytesMut::from("hello world");
        match reader.read(&mut buf) {
            ReadResult::Complete(body) => {
                assert_eq!(&body[..], b"hello");
                assert_eq!(&buf[..], b" world");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn test_body_reader_content_length_need_more() {
        let headers = vec![("Content-Length".into(), "10".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        let mut buf = BytesMut::from("hello");
        assert!(matches!(reader.read(&mut buf), ReadResult::NeedMore));
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn test_body_reader_chunked_simple() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        let mut buf = BytesMut::from("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n");

        match reader.read(&mut buf) {
            ReadResult::ChunkDecoded(c) => assert_eq!(&c[..], b"hello"),
            other => panic!("expected ChunkDecoded, got {other:?}"),
        }
        match reader.read(&mut buf) {
            ReadResult::ChunkDecoded(c) => assert_eq!(&c[..], b" world"),
            other => panic!("expected ChunkDecoded, got {other:?}"),
        }
        match reader.read(&mut buf) {
            ReadResult::Complete(body) => assert_eq!(&body[..], b"hello world"),
            other => panic!("expected Complete, got {other:?}"),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn test_body_reader_chunked_trailer() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        let mut buf =
            BytesMut::from("5\r\nhello\r\n0\r\nExpires: tomorrow\r\nX-Foo: bar\r\n\r\nNEXT");

        match reader.read(&mut buf) {
            ReadResult::ChunkDecoded(c) => assert_eq!(&c[..], b"hello"),
            other => panic!("expected ChunkDecoded, got {other:?}"),
        }
        match reader.read(&mut buf) {
            ReadResult::Complete(body) => assert_eq!(&body[..], b"hello"),
            other => panic!("expected Complete, got {other:?}"),
        }
        assert_eq!(&buf[..], b"NEXT");
    }

    #[test]
    fn test_body_reader_chunked_trailer_need_more() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        let mut buf = BytesMut::from("0\r\nExpires: tomor");
        assert!(matches!(reader.read(&mut buf), ReadResult::NeedMore));
        assert_eq!(&buf[..], b"0\r\nExpires: tomor");
    }

    #[test]
    fn test_body_reader_close_delimited() {
        let mut reader = BodyReader::new_for_response(200, "GET", &[]);
        let mut buf = BytesMut::from("some data");

        match reader.read(&mut buf) {
            ReadResult::ChunkDecoded(c) => assert_eq!(&c[..], b"some data"),
            other => panic!("expected ChunkDecoded, got {other:?}"),
        }
        assert!(buf.is_empty());
        assert!(matches!(reader.read(&mut buf), ReadResult::NeedMore));

        buf.extend_from_slice(b" more");
        let body = reader.finish(&mut buf);
        assert_eq!(&body[..], b"some data more");
    }

    #[test]
    fn test_body_reader_request_chunked() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_request(&headers);
        let mut buf = BytesMut::from("e\r\n{\"hello\":true}\r\n0\r\n\r\n");

        match reader.read(&mut buf) {
            ReadResult::ChunkDecoded(c) => assert_eq!(&c[..], b"{\"hello\":true}"),
            other => panic!("expected ChunkDecoded, got {other:?}"),
        }
        match reader.read(&mut buf) {
            ReadResult::Complete(body) => assert_eq!(&body[..], b"{\"hello\":true}"),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn test_body_reader_request_no_body() {
        let headers = vec![("Host".into(), "localhost".into())];
        let mut reader = BodyReader::new_for_request(&headers);
        let mut buf = BytesMut::new();
        assert!(matches!(reader.read(&mut buf), ReadResult::Complete(_)));
    }

    // ── SseParser unit tests ──

    #[test]
    fn test_sse_parser_single_event() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push(
            "event: message_start\ndata: {\"type\":\"start\"}\n\n",
            &fk,
            ca,
            sa,
            0,
            &mut output,
        );
        assert_eq!(output.len(), 1);
        match &output[0] {
            HttpParseEvent::SseEvent(e) => {
                assert_eq!(e.event_type, "message_start");
                assert_eq!(e.data, "{\"type\":\"start\"}");
            }
            _ => panic!("expected SseEvent"),
        }
    }

    #[test]
    fn test_sse_parser_multiple_events() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push(
            "event: a\ndata: 1\n\nevent: b\ndata: 2\n\n",
            &fk,
            ca,
            sa,
            0,
            &mut output,
        );
        assert_eq!(output.len(), 2);
    }

    #[test]
    fn test_sse_parser_across_chunks() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push("event: delta\nda", &fk, ca, sa, 0, &mut output);
        assert!(output.is_empty());

        parser.push("ta: hello\n\n", &fk, ca, sa, 0, &mut output);
        assert_eq!(output.len(), 1);
        match &output[0] {
            HttpParseEvent::SseEvent(e) => {
                assert_eq!(e.event_type, "delta");
                assert_eq!(e.data, "hello");
            }
            _ => panic!("expected SseEvent"),
        }
    }

    #[test]
    fn test_sse_parser_flush_residual() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push("data: final", &fk, ca, sa, 0, &mut output);
        assert!(output.is_empty());

        parser.flush(&fk, ca, sa, 0, &mut output);
        assert_eq!(output.len(), 1);
        match &output[0] {
            HttpParseEvent::SseEvent(e) => assert_eq!(e.data, "final"),
            _ => panic!("expected SseEvent"),
        }
    }

    #[test]
    fn test_sse_parser_comment_ignored() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push(
            ": keep-alive\n\ndata: real\n\n",
            &fk,
            ca,
            sa,
            0,
            &mut output,
        );
        assert_eq!(output.len(), 1);
        match &output[0] {
            HttpParseEvent::SseEvent(e) => assert_eq!(e.data, "real"),
            _ => panic!("expected SseEvent"),
        }
    }

    // ── Integration tests for correctness fixes ──

    #[test]
    fn test_204_no_content() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/check HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        );
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 204 No Content\r\nDate: Thu, 01 Jan 2026 00:00:00 GMT\r\n\r\n",
        );
        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        assert_eq!(output.len(), 2);
        match &output[1] {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 204);
                assert!(resp.body.is_empty());
            }
            _ => panic!("expected HttpResponse"),
        }
        assert!(server_buf.is_empty());
    }

    #[test]
    fn test_304_not_modified() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("GET /data HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut server_buf = BytesMut::from("HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n");
        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        assert_eq!(output.len(), 2);
        match &output[1] {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 304);
                assert!(resp.body.is_empty());
            }
            _ => panic!("expected HttpResponse"),
        }
    }

    #[test]
    fn test_head_response() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("HEAD /data HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut server_buf = BytesMut::from("HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        assert_eq!(output.len(), 2);
        match &output[0] {
            HttpParseEvent::HttpRequest(req) => assert_eq!(req.method, "HEAD"),
            _ => panic!("expected HttpRequest"),
        }
        match &output[1] {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 200);
                assert!(resp.body.is_empty());
            }
            _ => panic!("expected HttpResponse"),
        }
    }

    #[test]
    fn test_chunked_request_body() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/chat HTTP/1.1\r\n\
             Host: localhost\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             e\r\n{\"hello\":true}\r\n0\r\n\r\n",
        );
        let mut server_buf =
            BytesMut::from("HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"world\":true}");
        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        assert_eq!(output.len(), 2);
        match &output[0] {
            HttpParseEvent::HttpRequest(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(&req.body[..], b"{\"hello\":true}");
            }
            _ => panic!("expected HttpRequest"),
        }
        assert!(client_buf.is_empty());
    }

    #[test]
    fn test_chunked_response_with_trailer() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("GET /data HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             5\r\nhello\r\n\
             0\r\n\
             Expires: tomorrow\r\n\
             X-Checksum: abc123\r\n\
             \r\n",
        );
        let mut output = Vec::new();
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        assert_eq!(output.len(), 2);
        match &output[1] {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(&resp.body[..], b"hello");
            }
            _ => panic!("expected HttpResponse"),
        }
        assert!(server_buf.is_empty());
    }

    #[test]
    fn test_keep_alive_two_rounds() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut output = Vec::new();

        // Round 1
        let mut client_buf = BytesMut::from("GET /first HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut server_buf = BytesMut::from("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            100,
            200,
            200,
            &mut output,
        );
        assert_eq!(output.len(), 2);

        // Round 2 on same connection
        client_buf.extend_from_slice(
            b"POST /second HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\ndata",
        );
        server_buf.extend_from_slice(b"HTTP/1.1 201 Created\r\nContent-Length: 7\r\n\r\ncreated");
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            300,
            400,
            400,
            &mut output,
        );
        assert_eq!(output.len(), 4);

        match &output[2] {
            HttpParseEvent::HttpRequest(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.uri, "/second");
            }
            _ => panic!("expected HttpRequest"),
        }
        match &output[3] {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 201);
                assert_eq!(&resp.body[..], b"created");
            }
            _ => panic!("expected HttpResponse"),
        }
    }

    #[test]
    fn test_non_chunked_sse() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n");

        let sse_body =
            "event: start\ndata: {\"type\":\"start\"}\n\nevent: delta\ndata: {\"text\":\"Hi\"}\n\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Content-Length: {}\r\n\
             \r\n\
             {sse_body}",
            sse_body.len()
        );
        let mut server_buf = BytesMut::from(resp.as_str());
        let mut output = Vec::new();

        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        let sse_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::SseEvent(_)))
            .count();
        let resp_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
            .count();

        assert_eq!(
            sse_count, 2,
            "expected 2 SSE events from non-chunked response"
        );
        assert_eq!(resp_count, 1);
    }

    #[test]
    fn test_close_delimited_finish() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("GET /data HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let mut server_buf =
            BytesMut::from("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\npartial data");
        let mut output = Vec::new();

        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );
        let resp_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
            .count();
        assert_eq!(resp_count, 0, "response should not be emitted yet");

        server_buf.extend_from_slice(b" and more");
        parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );
        let resp_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
            .count();
        assert_eq!(resp_count, 0, "still not emitted");

        parser.finish_response(&mut server_buf, &fk, ca, sa, 0, &mut output);
        let resp_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
            .count();
        assert_eq!(resp_count, 1);
        match output.last().unwrap() {
            HttpParseEvent::HttpResponse(resp) => {
                assert_eq!(&resp.body[..], b"partial data and more");
            }
            _ => panic!("expected HttpResponse"),
        }
    }

    #[test]
    fn test_body_reader_chunked_invalid_size() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        let mut buf = BytesMut::from("NOT_HEX\r\ndata\r\n0\r\n\r\n");
        assert!(matches!(reader.read(&mut buf), ReadResult::Error));
    }

    #[test]
    fn test_resync_on_corrupt_request_header() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from("\x00\x01\x02\r\n\r\n");
        let mut server_buf = BytesMut::new();
        let mut output = Vec::new();
        let result = parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );
        assert_eq!(result, ParseResult::NeedResync);
    }

    #[test]
    fn test_resync_on_corrupt_response_header() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        );
        let mut server_buf = BytesMut::from("\x00\x01\x02\r\n\r\n");
        let mut output = Vec::new();
        let result = parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );
        assert_eq!(output.len(), 1); // HttpRequest emitted
        assert_eq!(result, ParseResult::NeedResync);
    }

    #[test]
    fn test_resync_on_corrupt_response_chunk() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        );
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nNOT_HEX\r\ndata\r\n",
        );
        let mut output = Vec::new();
        let result = parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );
        assert_eq!(result, ParseResult::NeedResync);
    }

    #[test]
    fn test_resync_on_corrupt_request_chunk() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\nBADHEX\r\ndata\r\n",
        );
        let mut server_buf = BytesMut::new();
        let mut output = Vec::new();
        let result = parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );
        assert_eq!(result, ParseResult::NeedResync);
    }

    /// Two sequential POST + SSE chunked response cycles on the same flow
    /// (HTTP/1.1 keepalive, no pipelining). This mirrors what
    /// `tests/fixtures/keepalive_2sse_pipelined.pcap` shows: agent reuses one
    /// TCP connection for back-to-back chat completions. Parser must emit
    /// 2 HttpRequest and 2 HttpResponse events; SSE event order across the
    /// two responses must be preserved.
    #[test]
    fn test_keepalive_two_sse_chunked_responses_one_flow() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();

        let req1 = "POST /v1/chat/completions HTTP/1.1\r\n\
                    Host: localhost\r\n\
                    Content-Length: 2\r\n\
                    \r\n\
                    {}";
        let req2 = "POST /v1/chat/completions HTTP/1.1\r\n\
                    Host: localhost\r\n\
                    Content-Length: 2\r\n\
                    \r\n\
                    []";

        let sse_body_1 = "event: message_start\ndata: {\"id\":\"r1\"}\n\n\
                          event: content_block_delta\ndata: {\"text\":\"one\"}\n\n\
                          event: message_stop\ndata: {}\n\n";
        let sse_body_2 = "event: message_start\ndata: {\"id\":\"r2\"}\n\n\
                          event: content_block_delta\ndata: {\"text\":\"two\"}\n\n\
                          event: message_stop\ndata: {}\n\n";
        let chunk1 = format!("{:x}\r\n{}\r\n0\r\n\r\n", sse_body_1.len(), sse_body_1);
        let chunk2 = format!("{:x}\r\n{}\r\n0\r\n\r\n", sse_body_2.len(), sse_body_2);
        let resp1 = format!(
            "HTTP/1.1 200 OK\r\n\
             Transfer-Encoding: chunked\r\n\
             Content-Type: text/event-stream\r\n\
             \r\n{}",
            chunk1
        );
        let resp2 = format!(
            "HTTP/1.1 200 OK\r\n\
             Transfer-Encoding: chunked\r\n\
             Content-Type: text/event-stream\r\n\
             \r\n{}",
            chunk2
        );

        let mut client_buf = BytesMut::from(format!("{}{}", req1, req2).as_bytes());
        let mut server_buf = BytesMut::from(format!("{}{}", resp1, resp2).as_bytes());

        let mut output = Vec::new();
        let result = parser.parse(
            &mut client_buf,
            &mut server_buf,
            &fk,
            ca,
            sa,
            0,
            0,
            0,
            &mut output,
        );

        assert_eq!(result, ParseResult::Ok, "parse should not need resync");
        let req_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
            .count();
        let resp_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpResponse(_)))
            .count();
        let sse_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::SseEvent(_)))
            .count();
        assert_eq!(
            req_count, 2,
            "expected 2 HttpRequest events, got {}; output: {:?}",
            req_count, output
        );
        assert_eq!(
            resp_count, 2,
            "expected 2 HttpResponse events, got {}; output: {:?}",
            resp_count, output
        );
        assert_eq!(
            sse_count, 6,
            "expected 6 SSE events (3 per response), got {}",
            sse_count
        );

        // Order check: request → SSE×3 → response → request → SSE×3 → response.
        let kinds: Vec<&'static str> = output
            .iter()
            .map(|e| match e {
                HttpParseEvent::HttpRequest(_) => "REQ",
                HttpParseEvent::HttpResponse(_) => "RESP",
                HttpParseEvent::SseEvent(_) => "SSE",
                HttpParseEvent::Heartbeat { .. } => "HB",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["REQ", "SSE", "SSE", "SSE", "RESP", "REQ", "SSE", "SSE", "SSE", "RESP"],
            "event order mismatch"
        );
    }
}
