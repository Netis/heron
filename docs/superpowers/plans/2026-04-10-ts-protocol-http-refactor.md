# ts-protocol HTTP Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor `http.rs` to fix 4 correctness bugs and restructure HttpParser into BodyReader + SseParser for clarity and extensibility.

**Architecture:** Single-file refactor of `server/ts-protocol/src/http.rs`. Extract body framing logic into `BodyReader` struct, SSE parsing into `SseParser` struct. HttpParser state machine simplified from 5 to 4 states. Tiny hook added in `tcp.rs` for close-delimited response flush.

**Tech Stack:** Rust, bytes crate, httparse

**Spec:** `docs/superpowers/specs/2026-04-10-ts-protocol-http-refactor-design.md`

---

### Task 1: Implement BodyReader

**Files:**
- Modify: `server/ts-protocol/src/http.rs`

- [ ] **Step 1: Write failing tests for BodyReader**

Add these tests at the bottom of the `#[cfg(test)] mod tests` block in `http.rs`. They test the BodyReader in isolation.

```rust
    // ── BodyReader unit tests ──

    #[test]
    fn test_body_reader_no_body() {
        let mut reader = BodyReader::new_for_response(204, "POST", &[]);
        let mut buf = BytesMut::from("leftover");
        match reader.read(&mut buf) {
            ReadResult::Complete(body) => assert!(body.is_empty()),
            other => panic!("expected Complete, got {other:?}"),
        }
        // buf untouched
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
        assert_eq!(buf.len(), 5); // not consumed
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
        let mut buf = BytesMut::from(
            "5\r\nhello\r\n0\r\nExpires: tomorrow\r\nX-Foo: bar\r\n\r\nNEXT",
        );

        match reader.read(&mut buf) {
            ReadResult::ChunkDecoded(c) => assert_eq!(&c[..], b"hello"),
            other => panic!("expected ChunkDecoded, got {other:?}"),
        }
        match reader.read(&mut buf) {
            ReadResult::Complete(body) => assert_eq!(&body[..], b"hello"),
            other => panic!("expected Complete, got {other:?}"),
        }
        // Trailer consumed, only "NEXT" remains
        assert_eq!(&buf[..], b"NEXT");
    }

    #[test]
    fn test_body_reader_chunked_trailer_need_more() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        // Terminal chunk present but trailer not complete
        let mut buf = BytesMut::from("0\r\nExpires: tomor");
        assert!(matches!(reader.read(&mut buf), ReadResult::NeedMore));
        // Nothing consumed — will retry when more data arrives
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

        // No more data → NeedMore
        assert!(matches!(reader.read(&mut buf), ReadResult::NeedMore));

        // finish drains remaining
        buf.extend_from_slice(b" more");
        let body = reader.finish(&mut buf);
        assert_eq!(&body[..], b"some data more");
    }

    #[test]
    fn test_body_reader_request_chunked() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_request(&headers);
        let mut buf = BytesMut::from("d\r\n{\"hello\":true}\r\n0\r\n\r\n");

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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ts-protocol --manifest-path server/Cargo.toml 2>&1 | tail -5`
Expected: compilation errors — `BodyReader`, `ReadResult` not found.

- [ ] **Step 3: Implement BodyReader**

Add these types and impl above the `HttpParser` struct in `http.rs`, after the existing `BodyFraming` enum. Replace the existing `BodyFraming` enum with the new one:

```rust
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
}

/// Reads an HTTP message body according to its framing.
struct BodyReader {
    framing: BodyFraming,
    /// Accumulated decoded body (used by Chunked and CloseDelimited).
    decoded_body: BytesMut,
}

impl BodyReader {
    fn new() -> Self {
        Self {
            framing: BodyFraming::NoBody,
            decoded_body: BytesMut::new(),
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
        }
    }

    fn read(&mut self, buf: &mut BytesMut) -> ReadResult {
        match self.framing {
            BodyFraming::NoBody => ReadResult::Complete(Bytes::new()),
            BodyFraming::ContentLength(len) => {
                if buf.len() >= len {
                    let body = Bytes::copy_from_slice(&buf[..len]);
                    let _ = buf.split_to(len);
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
                    let data = buf.split();
                    self.decoded_body.extend_from_slice(&data);
                    ReadResult::ChunkDecoded(data.freeze())
                }
            }
        }
    }

    fn read_chunk(&mut self, buf: &mut BytesMut) -> ReadResult {
        loop {
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
                    // Malformed chunk line — skip and try next.
                    let _ = buf.split_to(line_end + 2);
                    continue;
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
                                return ReadResult::Complete(self.decoded_body.split().freeze());
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
            let chunk_data = buf.split_to(chunk_size);
            let _ = buf.split_to(2);
            self.decoded_body.extend_from_slice(&chunk_data);
            return ReadResult::ChunkDecoded(chunk_data.freeze());
        }
    }

    /// Flush remaining data as body on connection close.
    fn finish(&mut self, buf: &mut BytesMut) -> Bytes {
        match self.framing {
            BodyFraming::NoBody => Bytes::new(),
            BodyFraming::ContentLength(_) => buf.split().freeze(),
            BodyFraming::Chunked | BodyFraming::CloseDelimited => {
                if !buf.is_empty() {
                    self.decoded_body.extend_from_slice(&buf.split());
                }
                self.decoded_body.split().freeze()
            }
        }
    }

    /// Whether body data was fed incrementally via `ChunkDecoded`.
    fn was_incremental(&self) -> bool {
        matches!(self.framing, BodyFraming::Chunked | BodyFraming::CloseDelimited)
    }

    fn is_no_body(&self) -> bool {
        self.framing == BodyFraming::NoBody
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ts-protocol --manifest-path server/Cargo.toml -- test_body_reader 2>&1 | tail -20`
Expected: all `test_body_reader_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add server/ts-protocol/src/http.rs
git commit -m "feat(ts-protocol): add BodyReader with correct framing for all body types"
```

---

### Task 2: Implement SseParser

**Files:**
- Modify: `server/ts-protocol/src/http.rs`

- [ ] **Step 1: Write failing tests for SseParser**

Add to the test module:

```rust
    // ── SseParser unit tests ──

    #[test]
    fn test_sse_parser_single_event() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push("event: message_start\ndata: {\"type\":\"start\"}\n\n", &fk, ca, sa, 0, &mut output);
        assert_eq!(output.len(), 1);
        match &output[0] {
            ProtocolEvent::SseEvent(e) => {
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
            &fk, ca, sa, 0, &mut output,
        );
        assert_eq!(output.len(), 2);
    }

    #[test]
    fn test_sse_parser_across_chunks() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push("event: delta\nda", &fk, ca, sa, 0, &mut output);
        assert!(output.is_empty()); // incomplete

        parser.push("ta: hello\n\n", &fk, ca, sa, 0, &mut output);
        assert_eq!(output.len(), 1);
        match &output[0] {
            ProtocolEvent::SseEvent(e) => {
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
            ProtocolEvent::SseEvent(e) => assert_eq!(e.data, "final"),
            _ => panic!("expected SseEvent"),
        }
    }

    #[test]
    fn test_sse_parser_comment_ignored() {
        let (fk, ca, sa) = test_flow();
        let mut parser = SseParser::new();
        let mut output = Vec::new();

        parser.push(": keep-alive\n\ndata: real\n\n", &fk, ca, sa, 0, &mut output);
        // Comment-only block produces nothing; data block produces one event
        assert_eq!(output.len(), 1);
        match &output[0] {
            ProtocolEvent::SseEvent(e) => assert_eq!(e.data, "real"),
            _ => panic!("expected SseEvent"),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ts-protocol --manifest-path server/Cargo.toml 2>&1 | tail -5`
Expected: compilation error — `SseParser` not found.

- [ ] **Step 3: Implement SseParser**

Add above `HttpParser` in `http.rs`:

```rust
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
        output: &mut Vec<ProtocolEvent>,
    ) {
        self.residual.push_str(text);

        loop {
            // SSE events are separated by blank lines.
            let (sep_pos, skip) =
                if let Some(pos) = self.residual.find("\r\n\r\n") {
                    (pos, 4)
                } else if let Some(pos) = self.residual.find("\n\n") {
                    (pos, 2)
                } else {
                    break;
                };

            let event_text = self.residual[..sep_pos].to_string();
            self.residual = self.residual[sep_pos + skip..].to_string();

            if let Some(evt) = Self::parse_event(&event_text, flow_key, client_addr, server_addr, timestamp) {
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
        output: &mut Vec<ProtocolEvent>,
    ) {
        let residual = std::mem::take(&mut self.residual);
        let trimmed = residual.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(evt) = Self::parse_event(trimmed, flow_key, client_addr, server_addr, timestamp) {
            output.push(evt);
        }
    }

    fn parse_event(
        text: &str,
        flow_key: &FlowKey,
        client_addr: (IpAddr, u16),
        server_addr: (IpAddr, u16),
        timestamp: i64,
    ) -> Option<ProtocolEvent> {
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

        Some(ProtocolEvent::SseEvent(SseEventData {
            flow_key: flow_key.clone(),
            client_addr,
            server_addr,
            event_type,
            data: data_parts.join("\n"),
            timestamp_us: timestamp,
        }))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ts-protocol --manifest-path server/Cargo.toml -- test_sse_parser 2>&1 | tail -20`
Expected: all `test_sse_parser_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add server/ts-protocol/src/http.rs
git commit -m "feat(ts-protocol): add SseParser as independent SSE event parser"
```

---

### Task 3: Refactor HttpParser to use BodyReader + SseParser

**Files:**
- Modify: `server/ts-protocol/src/http.rs`

- [ ] **Step 1: Rewrite HttpParser**

Replace the `ParserState` enum, `HttpParser` struct, and all its `impl` methods. Keep the helper functions (`find_crlf`, `is_chunked`, `is_sse`, `extract_content_length`) and delete `looks_like_response_complete`. Delete the old `ChunkedResult` enum.

New `ParserState` (replaces old):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    WaitingForRequest,
    ReadingRequestBody,
    WaitingForResponse,
    ReadingResponseBody,
}
```

New `HttpParser` struct (replaces old):

```rust
pub struct HttpParser {
    state: ParserState,

    // Pending request data.
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
```

New `impl HttpParser`:

```rust
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
        output: &mut Vec<ProtocolEvent>,
    ) {
        'outer: loop {
            match self.state {
                ParserState::WaitingForRequest => {
                    if !self.try_parse_request_headers(client_buf, client_ts) {
                        break;
                    }
                    self.body_reader = BodyReader::new_for_request(&self.pending_req_headers);
                    self.state = ParserState::ReadingRequestBody;
                }
                ParserState::ReadingRequestBody => {
                    loop {
                        match self.body_reader.read(client_buf) {
                            ReadResult::ChunkDecoded(_) => continue,
                            ReadResult::Complete(body) => {
                                output.push(ProtocolEvent::HttpRequest(HttpRequestData {
                                    flow_key: flow_key.clone(),
                                    client_addr,
                                    server_addr,
                                    method: std::mem::take(&mut self.pending_method),
                                    uri: std::mem::take(&mut self.pending_uri),
                                    version: self.pending_req_version,
                                    headers: std::mem::take(&mut self.pending_req_headers),
                                    body,
                                    timestamp_us: self.pending_req_timestamp,
                                }));
                                self.state = ParserState::WaitingForResponse;
                                break;
                            }
                            ReadResult::NeedMore => break 'outer,
                        }
                    }
                }
                ParserState::WaitingForResponse => {
                    if !self.try_parse_response_headers(server_buf, server_ts) {
                        break;
                    }
                    self.body_reader = BodyReader::new_for_response(
                        self.pending_resp_status,
                        &self.last_request_method,
                        &self.pending_resp_headers,
                    );
                    if self.pending_resp_is_sse {
                        self.sse_parser.reset();
                    }

                    // NoBody responses complete immediately.
                    if self.body_reader.is_no_body() {
                        output.push(ProtocolEvent::HttpResponse(HttpResponseData {
                            flow_key: flow_key.clone(),
                            client_addr,
                            server_addr,
                            status: self.pending_resp_status,
                            version: self.pending_resp_version,
                            headers: std::mem::take(&mut self.pending_resp_headers),
                            body: Bytes::new(),
                            first_byte_timestamp_us: self.pending_resp_timestamp,
                            complete_timestamp_us: server_ts,
                        }));
                        self.state = ParserState::WaitingForRequest;
                        continue;
                    }

                    self.state = ParserState::ReadingResponseBody;
                }
                ParserState::ReadingResponseBody => {
                    loop {
                        match self.body_reader.read(server_buf) {
                            ReadResult::ChunkDecoded(chunk) => {
                                if self.pending_resp_is_sse {
                                    if let Ok(text) = std::str::from_utf8(&chunk) {
                                        self.sse_parser.push(
                                            text, flow_key, client_addr, server_addr,
                                            server_last_ts, output,
                                        );
                                    }
                                }
                            }
                            ReadResult::Complete(body) => {
                                if self.pending_resp_is_sse {
                                    if !self.body_reader.was_incremental() {
                                        if let Ok(text) = std::str::from_utf8(&body) {
                                            self.sse_parser.push(
                                                text, flow_key, client_addr, server_addr,
                                                server_last_ts, output,
                                            );
                                        }
                                    }
                                    self.sse_parser.flush(
                                        flow_key, client_addr, server_addr,
                                        server_last_ts, output,
                                    );
                                }
                                output.push(ProtocolEvent::HttpResponse(HttpResponseData {
                                    flow_key: flow_key.clone(),
                                    client_addr,
                                    server_addr,
                                    status: self.pending_resp_status,
                                    version: self.pending_resp_version,
                                    headers: std::mem::take(&mut self.pending_resp_headers),
                                    body,
                                    first_byte_timestamp_us: self.pending_resp_timestamp,
                                    complete_timestamp_us: server_last_ts,
                                }));
                                self.state = ParserState::WaitingForRequest;
                                break;
                            }
                            ReadResult::NeedMore => break 'outer,
                        }
                    }
                }
            }
        }
    }

    /// Flush a pending response when the connection closes.
    /// Called by TcpFlow on FIN/RST.
    pub fn finish_response(
        &mut self,
        server_buf: &mut BytesMut,
        flow_key: &FlowKey,
        client_addr: (IpAddr, u16),
        server_addr: (IpAddr, u16),
        server_last_ts: i64,
        output: &mut Vec<ProtocolEvent>,
    ) {
        if self.state != ParserState::ReadingResponseBody {
            return;
        }
        let body = self.body_reader.finish(server_buf);
        if self.pending_resp_is_sse {
            if !self.body_reader.was_incremental() {
                if let Ok(text) = std::str::from_utf8(&body) {
                    self.sse_parser.push(
                        text, flow_key, client_addr, server_addr,
                        server_last_ts, output,
                    );
                }
            }
            self.sse_parser.flush(
                flow_key, client_addr, server_addr,
                server_last_ts, output,
            );
        }
        output.push(ProtocolEvent::HttpResponse(HttpResponseData {
            flow_key: flow_key.clone(),
            client_addr,
            server_addr,
            status: self.pending_resp_status,
            version: self.pending_resp_version,
            headers: std::mem::take(&mut self.pending_resp_headers),
            body,
            first_byte_timestamp_us: self.pending_resp_timestamp,
            complete_timestamp_us: server_last_ts,
        }));
        self.state = ParserState::WaitingForRequest;
    }

    fn try_parse_request_headers(&mut self, buf: &mut BytesMut, timestamp: i64) -> bool {
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
                true
            }
            Ok(httparse::Status::Partial) => false,
            Err(_) => {
                if !buf.is_empty() {
                    let _ = buf.split_to(1);
                }
                false
            }
        }
    }

    fn try_parse_response_headers(&mut self, buf: &mut BytesMut, timestamp: i64) -> bool {
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
                self.pending_resp_is_sse = is_sse(&self.pending_resp_headers);
                self.pending_resp_timestamp = timestamp;
                let _ = buf.split_to(header_len);
                true
            }
            Ok(httparse::Status::Partial) => false,
            Err(_) => {
                if !buf.is_empty() {
                    let _ = buf.split_to(1);
                }
                false
            }
        }
    }
}
```

Delete from `http.rs`:
- Old `ChunkedResult` enum
- `looks_like_response_complete` function

- [ ] **Step 2: Run all existing tests**

Run: `cargo test -p ts-protocol --manifest-path server/Cargo.toml 2>&1 | tail -30`
Expected: ALL tests pass (existing + new BodyReader + SseParser tests).

- [ ] **Step 3: Commit**

```bash
git add server/ts-protocol/src/http.rs
git commit -m "refactor(ts-protocol): rewrite HttpParser using BodyReader + SseParser"
```

---

### Task 4: Add tcp.rs finish_response hook

**Files:**
- Modify: `server/ts-protocol/src/tcp.rs`

- [ ] **Step 1: Add finish_response call on connection close**

In `TcpFlow::push()`, after the existing payload processing block (line ~146), add a call to flush pending responses when the connection is closing or closed:

```rust
        // Append payload if non-empty.
        if !pkt.payload.is_empty() {
            self.append_payload(pkt);
            self.try_parse_http(output);
        }

        // Flush pending response on connection close.
        if self.state == TcpState::Closing || self.state == TcpState::Closed {
            self.finish_pending_response(output);
        }
```

Add a new method to `TcpFlow`:

```rust
    fn finish_pending_response(&mut self, output: &mut Vec<ProtocolEvent>) {
        let (server_buf, client_addr, server_addr, server_last_ts) =
            match self.client_side {
                ClientSide::AtoB => (
                    &mut self.b_to_a_buf,
                    self.addr_a,
                    self.addr_b,
                    self.last_b_to_a_data_ts,
                ),
                ClientSide::BtoA => (
                    &mut self.a_to_b_buf,
                    self.addr_b,
                    self.addr_a,
                    self.last_a_to_b_data_ts,
                ),
                ClientSide::Unknown => return,
            };

        self.http_parser.finish_response(
            server_buf,
            &self.flow_key,
            client_addr,
            server_addr,
            server_last_ts,
            output,
        );
    }
```

Also fix the RST handler to flush before returning:

```rust
        if pkt.has_rst() {
            self.state = TcpState::Closed;
            self.finish_pending_response(output);
            return;
        }
```

- [ ] **Step 2: Run all tests**

Run: `cargo test -p ts-protocol --manifest-path server/Cargo.toml 2>&1 | tail -20`
Expected: all tests PASS.

- [ ] **Step 3: Commit**

```bash
git add server/ts-protocol/src/tcp.rs
git commit -m "feat(ts-protocol): flush pending response on connection close (FIN/RST)"
```

---

### Task 5: Add new integration tests

**Files:**
- Modify: `server/ts-protocol/src/http.rs`

- [ ] **Step 1: Add 204 No Content test**

```rust
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
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);

        assert_eq!(output.len(), 2);
        match &output[1] {
            ProtocolEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 204);
                assert!(resp.body.is_empty());
            }
            _ => panic!("expected HttpResponse"),
        }
        assert!(server_buf.is_empty());
    }
```

- [ ] **Step 2: Add 304 Not Modified test**

```rust
    #[test]
    fn test_304_not_modified() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "GET /data HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n",
        );
        let mut output = Vec::new();
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);

        assert_eq!(output.len(), 2);
        match &output[1] {
            ProtocolEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 304);
                assert!(resp.body.is_empty());
            }
            _ => panic!("expected HttpResponse"),
        }
    }
```

- [ ] **Step 3: Add HEAD response test**

```rust
    #[test]
    fn test_head_response() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "HEAD /data HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        // Server includes Content-Length but no body (per HTTP spec for HEAD).
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n",
        );
        let mut output = Vec::new();
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);

        assert_eq!(output.len(), 2);
        match &output[0] {
            ProtocolEvent::HttpRequest(req) => assert_eq!(req.method, "HEAD"),
            _ => panic!("expected HttpRequest"),
        }
        match &output[1] {
            ProtocolEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 200);
                assert!(resp.body.is_empty());
            }
            _ => panic!("expected HttpResponse"),
        }
    }
```

- [ ] **Step 4: Add chunked request body test**

```rust
    #[test]
    fn test_chunked_request_body() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "POST /v1/chat HTTP/1.1\r\n\
             Host: localhost\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             d\r\n{\"hello\":true}\r\n0\r\n\r\n",
        );
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"world\":true}",
        );
        let mut output = Vec::new();
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);

        assert_eq!(output.len(), 2);
        match &output[0] {
            ProtocolEvent::HttpRequest(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(&req.body[..], b"{\"hello\":true}");
            }
            _ => panic!("expected HttpRequest"),
        }
        assert!(client_buf.is_empty());
    }
```

- [ ] **Step 5: Add chunked trailer test**

```rust
    #[test]
    fn test_chunked_response_with_trailer() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "GET /data HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
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
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);

        assert_eq!(output.len(), 2);
        match &output[1] {
            ProtocolEvent::HttpResponse(resp) => {
                assert_eq!(&resp.body[..], b"hello");
            }
            _ => panic!("expected HttpResponse"),
        }
        // Trailer fully consumed
        assert!(server_buf.is_empty());
    }
```

- [ ] **Step 6: Add keep-alive two-round test**

```rust
    #[test]
    fn test_keep_alive_two_rounds() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut output = Vec::new();

        // Round 1
        let mut client_buf = BytesMut::from(
            "GET /first HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        );
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 100, 200, 200, &mut output);
        assert_eq!(output.len(), 2);

        // Round 2 on same connection
        client_buf.extend_from_slice(
            b"POST /second HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\ndata",
        );
        server_buf.extend_from_slice(
            b"HTTP/1.1 201 Created\r\nContent-Length: 7\r\n\r\ncreated",
        );
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 300, 400, 400, &mut output);
        assert_eq!(output.len(), 4);

        match &output[2] {
            ProtocolEvent::HttpRequest(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.uri, "/second");
            }
            _ => panic!("expected HttpRequest"),
        }
        match &output[3] {
            ProtocolEvent::HttpResponse(resp) => {
                assert_eq!(resp.status, 201);
                assert_eq!(&resp.body[..], b"created");
            }
            _ => panic!("expected HttpResponse"),
        }
    }
```

- [ ] **Step 7: Add non-chunked SSE test**

```rust
    #[test]
    fn test_non_chunked_sse() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );

        let sse_body = "event: start\ndata: {\"type\":\"start\"}\n\n\
                         event: delta\ndata: {\"text\":\"Hi\"}\n\n";
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

        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);

        let sse_count = output.iter().filter(|e| matches!(e, ProtocolEvent::SseEvent(_))).count();
        let resp_count = output.iter().filter(|e| matches!(e, ProtocolEvent::HttpResponse(_))).count();

        assert_eq!(sse_count, 2, "expected 2 SSE events from non-chunked response");
        assert_eq!(resp_count, 1);
    }
```

- [ ] **Step 8: Add close-delimited finish test**

```rust
    #[test]
    fn test_close_delimited_finish() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        let mut client_buf = BytesMut::from(
            "GET /data HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        // Response with no Content-Length and no chunked → CloseDelimited.
        let mut server_buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\npartial data",
        );
        let mut output = Vec::new();

        // First parse: request emitted, response body accumulates but no Complete yet.
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);
        let resp_count = output.iter().filter(|e| matches!(e, ProtocolEvent::HttpResponse(_))).count();
        assert_eq!(resp_count, 0, "response should not be emitted yet");

        // Simulate more data arriving.
        server_buf.extend_from_slice(b" and more");
        parser.parse(&mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output);
        let resp_count = output.iter().filter(|e| matches!(e, ProtocolEvent::HttpResponse(_))).count();
        assert_eq!(resp_count, 0, "still not emitted");

        // Connection close: finish_response flushes.
        parser.finish_response(&mut server_buf, &fk, ca, sa, 0, &mut output);
        let resp_count = output.iter().filter(|e| matches!(e, ProtocolEvent::HttpResponse(_))).count();
        assert_eq!(resp_count, 1);
        match output.last().unwrap() {
            ProtocolEvent::HttpResponse(resp) => {
                assert_eq!(&resp.body[..], b"partial data and more");
            }
            _ => panic!("expected HttpResponse"),
        }
    }
```

- [ ] **Step 9: Run all tests**

Run: `cargo test -p ts-protocol --manifest-path server/Cargo.toml 2>&1 | tail -40`
Expected: ALL tests pass.

- [ ] **Step 10: Commit**

```bash
git add server/ts-protocol/src/http.rs
git commit -m "test(ts-protocol): add integration tests for no-body, chunked req/trailer, SSE, close-delimited"
```
