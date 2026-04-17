# Mid-Stream HTTP Capture Resync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Handle mid-connection packet capture and ZMQ packet loss by adding dual-layer resync between TcpFlow and HttpParser.

**Architecture:** TCP layer does per-packet `looks_like_http_request()` inspection to gate buffer entry; HTTP layer returns `NeedResync` on unrecoverable parse errors. A `synced` bool on TcpFlow unifies with `ClientSide` determination.

**Tech Stack:** Rust, httparse, bytes crate, tracing, ts-common internal_metrics

---

### Task 1: Add `Resync` metric variant

**Files:**
- Modify: `server/ts-common/src/internal_metrics.rs:124-154` (define_metrics! macro)
- Modify: `server/ts-protocol/src/pipeline.rs:56-64` (worker metric registration)

- [ ] **Step 1: Add `Resync` to `define_metrics!`**

In `server/ts-common/src/internal_metrics.rs`, add after `SseEventsParsed`:

```rust
    HttpResyncEvents        => { kind: Counter, group: Pipeline, short: "http_resync"    },
```

- [ ] **Step 2: Register `Resync` metric for flow workers**

In `server/ts-protocol/src/pipeline.rs`, add `Metric::HttpResyncEvents` to the worker metrics array:

```rust
        let worker_metrics = metrics_sys.register_worker(
            &format!("worker.{i}"),
            &[
                Metric::NetPacketsParsed,
                Metric::HttpRequestsParsed,
                Metric::HttpResponsesParsed,
                Metric::SseEventsParsed,
                Metric::HttpResyncEvents,
            ],
        );
```

- [ ] **Step 3: Verify it compiles**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check -p ts-common -p ts-protocol`
Expected: compiles with no errors (metric is registered but not yet used)

- [ ] **Step 4: Commit**

```bash
git add server/ts-common/src/internal_metrics.rs server/ts-protocol/src/pipeline.rs
git commit -m "feat(ts-protocol): add HttpResyncEvents metric variant"
```

---

### Task 2: Add `ParseResult` enum and `BodyReader` error signaling in HttpParser

**Files:**
- Modify: `server/ts-protocol/src/http.rs`

This task changes the internal `BodyReader::read_chunk` to signal errors instead of skipping, adds the `ParseResult` return type, and adds the two new public methods. The `parse()` signature change and wiring comes in Task 3.

- [ ] **Step 1: Write failing test for `read_chunk` error**

Add to the `#[cfg(test)] mod tests` in `server/ts-protocol/src/http.rs`:

```rust
    #[test]
    fn test_body_reader_chunked_invalid_size() {
        let headers = vec![("Transfer-Encoding".into(), "chunked".into())];
        let mut reader = BodyReader::new_for_response(200, "GET", &headers);
        let mut buf = BytesMut::from("NOT_HEX\r\ndata\r\n0\r\n\r\n");
        assert!(matches!(reader.read(&mut buf), ReadResult::Error));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol -- test_body_reader_chunked_invalid_size`
Expected: FAIL — `ReadResult` has no `Error` variant

- [ ] **Step 3: Add `Error` variant to `ReadResult` and update `read_chunk`**

In `server/ts-protocol/src/http.rs`, add `Error` to `ReadResult`:

```rust
#[derive(Debug)]
enum ReadResult {
    NeedMore,
    Complete(Bytes),
    ChunkDecoded(Bytes),
    /// Unrecoverable decode error (e.g. invalid chunk size).
    Error,
}
```

In `read_chunk`, replace the `Err(_)` branch that skips:

```rust
            let chunk_size = match usize::from_str_radix(size_str, 16) {
                Ok(s) => s,
                Err(_) => {
                    return ReadResult::Error;
                }
            };
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol -- test_body_reader_chunked_invalid_size`
Expected: PASS

- [ ] **Step 5: Add `ParseResult` enum and new public methods**

Add above the `HttpParser` struct in `server/ts-protocol/src/http.rs`:

```rust
/// Result of an `HttpParser::parse()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseResult {
    /// Normal: parsed what was available, waiting for more data.
    Ok,
    /// Unrecoverable error in current req-resp cycle. Caller should resync.
    NeedResync,
}
```

Add to the `impl HttpParser` block:

```rust
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
```

- [ ] **Step 6: Verify it compiles**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check -p ts-protocol`
Expected: compiles (new types/methods exist but `parse()` still returns `()`)

- [ ] **Step 7: Commit**

```bash
git add server/ts-protocol/src/http.rs
git commit -m "feat(ts-protocol): add ParseResult, ReadResult::Error, and HttpParser reset/query methods"
```

---

### Task 3: Wire `ParseResult` into `HttpParser::parse()` and header parsing

**Files:**
- Modify: `server/ts-protocol/src/http.rs`

- [ ] **Step 1: Write failing tests for NeedResync on corrupt headers**

Add to the test module in `server/ts-protocol/src/http.rs`:

```rust
    #[test]
    fn test_resync_on_corrupt_request_header() {
        let (fk, ca, sa) = test_flow();
        let mut parser = HttpParser::new();
        // Garbage that is not a valid HTTP request and not Partial (no leading method).
        let mut client_buf = BytesMut::from("\x00\x01\x02\r\n\r\n");
        let mut server_buf = BytesMut::new();
        let mut output = Vec::new();
        let result = parser.parse(
            &mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output,
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
        // Garbage response.
        let mut server_buf = BytesMut::from("\x00\x01\x02\r\n\r\n");
        let mut output = Vec::new();
        let result = parser.parse(
            &mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output,
        );
        // Request should be parsed, then response fails.
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
            &mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output,
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
            &mut client_buf, &mut server_buf, &fk, ca, sa, 0, 0, 0, &mut output,
        );
        assert_eq!(result, ParseResult::NeedResync);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol -- test_resync_on_corrupt`
Expected: FAIL — `parse()` returns `()`, not `ParseResult`

- [ ] **Step 3: Change `parse()` return type and wire NeedResync**

In `server/ts-protocol/src/http.rs`, change the `parse()` signature:

```rust
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
    ) -> ParseResult {
```

Change `try_parse_request_headers` return type from `bool` to `Option<bool>` where `None` means error:

```rust
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
```

Same pattern for `try_parse_response_headers`:

```rust
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

                self.pending_resp_is_sse = is_sse(&self.pending_resp_headers);
                self.pending_resp_timestamp = timestamp;

                let _ = buf.split_to(header_len);
                Some(true)
            }
            Ok(httparse::Status::Partial) => Some(false),
            Err(_) => None,
        }
    }
```

Update the `parse()` body — the full state machine loop:

```rust
    ) -> ParseResult {
        'outer: loop {
            match self.state {
                ParserState::WaitingForRequest => {
                    match self.try_parse_request_headers(client_buf, client_ts) {
                        Some(true) => {}
                        Some(false) => break,
                        None => return ParseResult::NeedResync,
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
                            ReadResult::Error => return ParseResult::NeedResync,
                        }
                    }
                }
                ParserState::WaitingForResponse => {
                    match self.try_parse_response_headers(server_buf, server_ts) {
                        Some(true) => {}
                        Some(false) => break,
                        None => return ParseResult::NeedResync,
                    }
                    self.body_reader = BodyReader::new_for_response(
                        self.pending_resp_status,
                        &self.last_request_method,
                        &self.pending_resp_headers,
                    );
                    if self.pending_resp_is_sse {
                        self.sse_parser.reset();
                    }
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
                                continue;
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
                            ReadResult::Error => return ParseResult::NeedResync,
                        }
                    }
                }
            }
        }
        ParseResult::Ok
    }
```

- [ ] **Step 4: Fix the call site in `tcp.rs`**

In `server/ts-protocol/src/tcp.rs`, `try_parse_http` currently calls `self.http_parser.parse(...)` without using the return value. Temporarily ignore it so it compiles (Task 4 will wire it properly):

```rust
        let _ = self.http_parser.parse(
            client_buf,
            server_buf,
            &self.flow_key,
            client_addr,
            server_addr,
            client_ts.unwrap_or(0),
            server_ts.unwrap_or(0),
            server_last_ts,
            output,
        );
```

- [ ] **Step 5: Run all tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol`
Expected: ALL PASS — existing tests still pass, new resync tests pass

- [ ] **Step 6: Commit**

```bash
git add server/ts-protocol/src/http.rs server/ts-protocol/src/tcp.rs
git commit -m "feat(ts-protocol): wire ParseResult into HttpParser::parse(), return NeedResync on errors"
```

---

### Task 4: Rework TcpFlow with `synced` state and per-packet pre-check

**Files:**
- Modify: `server/ts-protocol/src/tcp.rs`

- [ ] **Step 1: Write failing tests for mid-stream resync scenarios**

Add to the bottom of `server/ts-protocol/src/tcp.rs` (the file currently has no test module, so add one):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crate::model::ProtocolEvent;
    use crate::net::{Direction, FlowKey, ParsedPacket, TCP_SYN, TCP_ACK, TCP_FIN, TCP_RST};

    fn make_pkt(
        flow_key: &FlowKey,
        direction: Direction,
        payload: &[u8],
        seq: u32,
        tcp_flags: u8,
    ) -> ParsedPacket {
        let (src_ip, src_port, dst_ip, dst_port) = match direction {
            Direction::AtoB => (flow_key.addr_a.0, flow_key.addr_a.1, flow_key.addr_b.0, flow_key.addr_b.1),
            Direction::BtoA => (flow_key.addr_b.0, flow_key.addr_b.1, flow_key.addr_a.0, flow_key.addr_a.1),
        };
        ParsedPacket {
            flow_key: flow_key.clone(),
            direction,
            src_ip,
            src_port,
            dst_ip,
            dst_port,
            tcp_flags,
            tcp_seq: seq,
            tcp_ack: 0,
            payload: Bytes::copy_from_slice(payload),
            timestamp_us: 0,
        }
    }

    fn test_flow_key() -> FlowKey {
        FlowKey::new(
            "10.0.0.1".parse().unwrap(), 5000,
            "10.0.0.2".parse().unwrap(), 8080,
        )
    }

    #[test]
    fn test_mid_stream_join_discards_server_data_then_syncs() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        // First packet is server-direction response data — should be discarded.
        let resp_data = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        flow.push(&make_pkt(&fk, Direction::BtoA, resp_data, 1000, 0), &mut output);
        assert!(output.is_empty(), "server data before sync should be discarded");

        // Client sends a valid request — should sync and parse.
        let req = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";
        flow.push(&make_pkt(&fk, Direction::AtoB, req, 100, 0), &mut output);
        assert_eq!(
            output.iter().filter(|e| matches!(e, ProtocolEvent::HttpRequest(_))).count(),
            1,
            "request should be parsed after sync"
        );
    }

    #[test]
    fn test_new_request_during_response_wait_triggers_resync() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        // SYN handshake.
        flow.push(&make_pkt(&fk, Direction::AtoB, &[], 0, TCP_SYN), &mut output); // SYN
        flow.push(&make_pkt(&fk, Direction::BtoA, &[], 0, TCP_SYN | TCP_ACK), &mut output); // SYN-ACK

        // First request.
        let req1 = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req1, 1, 0), &mut output);
        assert_eq!(output.len(), 1); // HttpRequest

        // No response arrives. Client sends a new request.
        let req2 = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req2, 100, 0), &mut output);

        // Second request should trigger resync and be parsed.
        let req_count = output.iter().filter(|e| matches!(e, ProtocolEvent::HttpRequest(_))).count();
        assert_eq!(req_count, 2, "second request should be parsed after resync");
    }

    #[test]
    fn test_http_parse_error_triggers_resync_then_recovers() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        // SYN.
        flow.push(&make_pkt(&fk, Direction::AtoB, &[], 0, 1), &mut output);
        flow.push(&make_pkt(&fk, Direction::BtoA, &[], 0, 0x11), &mut output);

        // Valid request.
        let req = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req, 1, 0), &mut output);
        assert_eq!(output.len(), 1);

        // Corrupt response (will cause NeedResync from HttpParser).
        let corrupt = b"\x00\x01\x02\r\n\r\n";
        flow.push(&make_pkt(&fk, Direction::BtoA, corrupt, 1000, 0), &mut output);
        // No new events — resync happened, parser reset.

        // Random server data — still unsynced, discarded.
        flow.push(&make_pkt(&fk, Direction::BtoA, b"more junk", 1100, 0), &mut output);

        // New valid request — should resync and parse.
        let req2 = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";
        flow.push(&make_pkt(&fk, Direction::AtoB, req2, 200, 0), &mut output);
        let req_count = output.iter().filter(|e| matches!(e, ProtocolEvent::HttpRequest(_))).count();
        assert_eq!(req_count, 2, "should recover after resync");
    }

    #[test]
    fn test_syn_handshake_sets_synced() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        flow.push(&make_pkt(&fk, Direction::AtoB, &[], 0, 1), &mut output);
        flow.push(&make_pkt(&fk, Direction::BtoA, &[], 0, 0x11), &mut output);

        // Data should be accepted without needing looks_like_http_request.
        let req = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req, 1, 0), &mut output);
        assert_eq!(output.len(), 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol -- tcp::tests`
Expected: FAIL — tests reference `ParsedPacket` fields (`syn`, `ack`, `fin`, `rst`) that may need checking, and TcpFlow doesn't have `synced` logic yet. The `test_mid_stream_join` test will fail because current code doesn't discard server data when unsynced.

Note: Check the actual `ParsedPacket` struct fields before running. The test helper `make_pkt` must match the real struct. Read `server/ts-protocol/src/net.rs` to verify field names and adjust the helper if needed.

- [ ] **Step 3: Add `synced` field and rework `push()` / `try_parse_http()`**

In `server/ts-protocol/src/tcp.rs`, add `synced: bool` to `TcpFlow`:

```rust
pub struct TcpFlow {
    state: TcpState,
    client_side: ClientSide,
    synced: bool,
    // ... rest unchanged
}
```

Initialize in `new()`:

```rust
    pub fn new(flow_key: FlowKey) -> Self {
        // ...
        Self {
            state: TcpState::Init,
            client_side: ClientSide::Unknown,
            synced: false,
            // ... rest unchanged
        }
    }
```

Make `looks_like_http_request` pub(crate):

```rust
pub(crate) fn looks_like_http_request(buf: &[u8]) -> bool {
```

Rework `push()`:

```rust
    pub fn push(
        &mut self,
        pkt: &ParsedPacket,
        output: &mut Vec<ProtocolEvent>,
    ) {
        // Handle RST.
        if pkt.has_rst() {
            self.state = TcpState::Closed;
            self.finish_pending_response(output);
            return;
        }

        // SYN (not SYN-ACK): determines client side and sets synced.
        if pkt.has_syn() && !pkt.has_ack() {
            self.state = TcpState::SynSent;
            self.client_side = match pkt.direction {
                Direction::AtoB => ClientSide::AtoB,
                Direction::BtoA => ClientSide::BtoA,
            };
            self.synced = true;
            match pkt.direction {
                Direction::AtoB => self.a_to_b_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
                Direction::BtoA => self.b_to_a_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
            }
            return;
        }

        // SYN-ACK.
        if pkt.has_syn() && pkt.has_ack() {
            self.state = TcpState::Established;
            match pkt.direction {
                Direction::AtoB => self.a_to_b_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
                Direction::BtoA => self.b_to_a_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
            }
            return;
        }

        // FIN handling.
        if pkt.has_fin() {
            self.state = match self.state {
                TcpState::Closing => TcpState::Closed,
                _ => TcpState::Closing,
            };
        }

        // Transition to Established on first data.
        if self.state == TcpState::Init || self.state == TcpState::SynSent {
            if !pkt.payload.is_empty() {
                self.state = TcpState::Established;
            }
        }

        if !pkt.payload.is_empty() {
            if !self.synced {
                // Unsynced: check if this packet starts a valid HTTP request.
                if looks_like_http_request(&pkt.payload) {
                    // Determine client side from this packet's direction.
                    self.client_side = match pkt.direction {
                        Direction::AtoB => ClientSide::AtoB,
                        Direction::BtoA => ClientSide::BtoA,
                    };
                    self.synced = true;
                    self.a_to_b_buf.clear();
                    self.b_to_a_buf.clear();
                    self.http_parser.reset();
                    self.append_payload(pkt);
                    self.try_parse_http(output);
                }
                // else: discard packet (don't append to buffer).
            } else {
                // Synced: check for "new request while waiting for response".
                let is_client_direction = match self.client_side {
                    ClientSide::AtoB => matches!(pkt.direction, Direction::AtoB),
                    ClientSide::BtoA => matches!(pkt.direction, Direction::BtoA),
                    ClientSide::Unknown => false,
                };

                if is_client_direction
                    && self.http_parser.is_waiting_for_response()
                    && looks_like_http_request(&pkt.payload)
                {
                    // Resync: abandon current round, start from this new request.
                    tracing::trace!(
                        flow = %self.flow_key,
                        "resync: new request arrived while waiting for response"
                    );
                    self.a_to_b_buf.clear();
                    self.b_to_a_buf.clear();
                    self.http_parser.reset();
                    self.a_to_b_next_seq = None;
                    self.b_to_a_next_seq = None;
                    self.first_a_to_b_data_ts = None;
                    self.first_b_to_a_data_ts = None;
                    self.append_payload(pkt);
                    self.try_parse_http(output);
                } else {
                    self.append_payload(pkt);
                    self.try_parse_http(output);
                }
            }
        }

        // Flush pending response on connection close.
        if self.state == TcpState::Closing || self.state == TcpState::Closed {
            self.finish_pending_response(output);
        }
    }
```

Update `try_parse_http` to handle `ParseResult::NeedResync`:

```rust
    fn try_parse_http(&mut self, output: &mut Vec<ProtocolEvent>) {
        let (client_buf, server_buf, client_addr, server_addr, client_ts, server_ts, server_last_ts) =
            match self.client_side {
                ClientSide::AtoB => (
                    &mut self.a_to_b_buf,
                    &mut self.b_to_a_buf,
                    self.addr_a,
                    self.addr_b,
                    self.first_a_to_b_data_ts,
                    self.first_b_to_a_data_ts,
                    self.last_b_to_a_data_ts,
                ),
                ClientSide::BtoA => (
                    &mut self.b_to_a_buf,
                    &mut self.a_to_b_buf,
                    self.addr_b,
                    self.addr_a,
                    self.first_b_to_a_data_ts,
                    self.first_a_to_b_data_ts,
                    self.last_a_to_b_data_ts,
                ),
                ClientSide::Unknown => return,
            };

        let result = self.http_parser.parse(
            client_buf,
            server_buf,
            &self.flow_key,
            client_addr,
            server_addr,
            client_ts.unwrap_or(0),
            server_ts.unwrap_or(0),
            server_last_ts,
            output,
        );

        if result == ParseResult::NeedResync {
            tracing::trace!(
                flow = %self.flow_key,
                "resync: HTTP parse error, waiting for next valid request"
            );
            self.synced = false;
            self.a_to_b_buf.clear();
            self.b_to_a_buf.clear();
            self.http_parser.reset();
        }
    }
```

Note: import `ParseResult` at the top of `tcp.rs`:

```rust
use crate::http::{HttpParser, ParseResult};
```

- [ ] **Step 4: Run tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol`
Expected: ALL PASS

- [ ] **Step 5: Add metrics increment for resync events**

In `server/ts-protocol/src/tcp.rs`, add a `metrics` field to `TcpFlow`:

Actually, `TcpFlow` doesn't currently hold metrics — `FlowWorker` holds the `MetricsWorker`. The simplest approach: have `push()` return whether a resync occurred, and let `FlowWorker::process` increment the counter.

Add a return value to `push()`:

```rust
    /// Process a parsed packet. Returns true if a resync event occurred.
    pub fn push(
        &mut self,
        pkt: &ParsedPacket,
        output: &mut Vec<ProtocolEvent>,
    ) -> bool {
```

Return `true` at each resync point (three places: unsynced→synced, synced new-request-during-response, NeedResync from parser). Return `false` at the end and all early returns.

In `try_parse_http`, change it to return `bool`:

```rust
    fn try_parse_http(&mut self, output: &mut Vec<ProtocolEvent>) -> bool {
        // ... existing code ...
        if result == ParseResult::NeedResync {
            // ... existing resync code ...
            return true;
        }
        false
    }
```

Then propagate in `push()`:

```rust
        // In the synced + new-request-while-waiting branch:
        resync_occurred = true;

        // In the normal path:
        if self.try_parse_http(output) {
            resync_occurred = true;
        }

        // ... at end of push():
        resync_occurred
```

In `FlowWorker::process` in `tcp.rs`:

```rust
    pub async fn process(&mut self, pkt: ParsedPacket) {
        self.metrics.counter(Metric::NetPacketsParsed).inc();

        let flow_key = pkt.flow_key.clone();
        let flow = self
            .flows
            .entry(flow_key.clone())
            .or_insert_with(|| TcpFlow::new(flow_key.clone()));

        let mut events = Vec::new();
        let resync = flow.push(&pkt, &mut events);
        if resync {
            self.metrics.counter(Metric::HttpResyncEvents).inc();
        }

        // ... rest unchanged
    }
```

- [ ] **Step 6: Run all tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol`
Expected: ALL PASS

- [ ] **Step 7: Commit**

```bash
git add server/ts-protocol/src/tcp.rs
git commit -m "feat(ts-protocol): add synced state and per-packet resync to TcpFlow"
```

---

### Task 5: Verify all existing tests still pass

**Files:** None (verification only)

- [ ] **Step 1: Run full workspace tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 2: Run clippy**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo clippy --workspace -- -D warnings`
Expected: No warnings

- [ ] **Step 3: Fix any issues found, then commit**

If any fixes needed:
```bash
git add -u
git commit -m "fix(ts-protocol): address clippy warnings from resync changes"
```
