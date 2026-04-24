use std::collections::HashMap;
use std::net::IpAddr;

use bytes::BytesMut;

use ts_common::internal_metrics::{Metric, MetricsWorker};

use crate::flow::WorkerInput;
use crate::http::{HttpParser, ParseResult};
use crate::model::HttpParseEvent;
use crate::net::{Direction, FlowKey, ParsedPacket};

/// Per-flow TCP connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TcpState {
    /// Waiting for SYN or first data packet.
    Init,
    /// SYN seen, waiting for SYN-ACK.
    SynSent,
    /// Connection established (handshake complete or data seen).
    Established,
    /// FIN seen from one side.
    Closing,
    /// Connection closed (FIN from both sides or RST).
    Closed,
}

/// Tracks which side is the client (the one that sends the HTTP request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientSide {
    /// Not yet determined.
    Unknown,
    /// The side sending in Direction::AtoB is the client.
    AtoB,
    /// The side sending in Direction::BtoA is the client.
    BtoA,
}

/// Per-flow TCP state and reassembly buffers.
pub struct TcpFlow {
    state: TcpState,
    client_side: ClientSide,
    /// Whether we are synchronized with the HTTP stream.
    /// `synced == true` implies `client_side != Unknown`.
    synced: bool,

    // Reassembly buffers for each direction.
    a_to_b_buf: BytesMut,
    b_to_a_buf: BytesMut,

    // Expected next sequence number for in-order reassembly.
    a_to_b_next_seq: Option<u32>,
    b_to_a_next_seq: Option<u32>,

    // Timestamp of the most recent data packet in each direction. Used as the
    // stamping time for HTTP requests/responses at parse time — on keep-alive
    // connections the connection's first-packet time is not a valid per-request
    // start time.
    last_a_to_b_data_ts: i64,
    last_b_to_a_data_ts: i64,

    /// Timestamp of the last packet received on this flow (any direction).
    last_pkt_ts: i64,

    // HTTP parser operates on the reassembled buffers.
    http_parser: HttpParser,

    // Connection identity for events.
    flow_key: FlowKey,
    addr_a: (IpAddr, u16),
    addr_b: (IpAddr, u16),
}

impl TcpFlow {
    pub fn new(flow_key: FlowKey) -> Self {
        let addr_a = flow_key.addr_a;
        let addr_b = flow_key.addr_b;
        Self {
            state: TcpState::Init,
            client_side: ClientSide::Unknown,
            synced: false,
            a_to_b_buf: BytesMut::new(),
            b_to_a_buf: BytesMut::new(),
            a_to_b_next_seq: None,
            b_to_a_next_seq: None,
            last_a_to_b_data_ts: 0,
            last_b_to_a_data_ts: 0,
            last_pkt_ts: 0,
            http_parser: HttpParser::new(),
            flow_key,
            addr_a,
            addr_b,
        }
    }

    /// Process a parsed packet belonging to this flow.
    /// Emits HttpParseEvents to the output vec.
    /// Returns `true` if a resync event occurred.
    pub fn push(&mut self, pkt: &ParsedPacket, output: &mut Vec<HttpParseEvent>) -> bool {
        let mut resync = false;
        self.last_pkt_ts = pkt.timestamp_us;

        // RST → close immediately.
        if pkt.has_rst() {
            self.state = TcpState::Closed;
            self.finish_pending_response(output);
            return false;
        }

        // SYN (no ACK) → client side determined, synced.
        if pkt.has_syn() && !pkt.has_ack() {
            self.state = TcpState::SynSent;
            self.client_side = match pkt.direction {
                Direction::AtoB => ClientSide::AtoB,
                Direction::BtoA => ClientSide::BtoA,
            };
            self.synced = true;
            // Initialize sequence tracking (next expected = SYN seq + 1).
            match pkt.direction {
                Direction::AtoB => self.a_to_b_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
                Direction::BtoA => self.b_to_a_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
            }
            return false;
        }

        // SYN-ACK.
        if pkt.has_syn() && pkt.has_ack() {
            self.state = TcpState::Established;
            match pkt.direction {
                Direction::AtoB => self.a_to_b_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
                Direction::BtoA => self.b_to_a_next_seq = Some(pkt.tcp_seq.wrapping_add(1)),
            }
            return false;
        }

        // FIN handling.
        if pkt.has_fin() {
            self.state = match self.state {
                TcpState::Closing => TcpState::Closed,
                _ => TcpState::Closing,
            };
            // FIN consumes one sequence number; process any remaining payload below.
        }

        // If we haven't seen a SYN, transition to Established on first data.
        if (self.state == TcpState::Init || self.state == TcpState::SynSent)
            && !pkt.payload.is_empty()
        {
            self.state = TcpState::Established;
        }

        // Process payload.
        if !pkt.payload.is_empty() {
            if !self.synced {
                // Not yet synced: look for an HTTP request to sync on.
                if looks_like_http_request(&pkt.payload) {
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
                    // We just synced from unsynced — that counts as a resync.
                    resync = true;
                }
                // else: discard (do nothing)
            } else {
                // Synced: check for new-request-while-waiting-for-response.
                let is_client = matches!(
                    (self.client_side, pkt.direction),
                    (ClientSide::AtoB, Direction::AtoB) | (ClientSide::BtoA, Direction::BtoA)
                );
                if is_client
                    && self.http_parser.is_waiting_for_response()
                    && looks_like_http_request(&pkt.payload)
                {
                    tracing::trace!(
                        flow = %self.flow_key,
                        "resync: new request while waiting for response"
                    );
                    self.a_to_b_buf.clear();
                    self.b_to_a_buf.clear();
                    self.http_parser.reset();
                    self.a_to_b_next_seq = None;
                    self.b_to_a_next_seq = None;
                    self.append_payload(pkt);
                    self.try_parse_http(output);
                    resync = true;
                } else {
                    self.append_payload(pkt);
                    resync |= self.try_parse_http(output);
                }
            }
        }

        // Flush pending response on connection close.
        if self.state == TcpState::Closing || self.state == TcpState::Closed {
            self.finish_pending_response(output);
        }

        resync
    }

    fn append_payload(&mut self, pkt: &ParsedPacket) {
        let (buf, next_seq, last_ts) = match pkt.direction {
            Direction::AtoB => (
                &mut self.a_to_b_buf,
                &mut self.a_to_b_next_seq,
                &mut self.last_a_to_b_data_ts,
            ),
            Direction::BtoA => (
                &mut self.b_to_a_buf,
                &mut self.b_to_a_next_seq,
                &mut self.last_b_to_a_data_ts,
            ),
        };

        *last_ts = pkt.timestamp_us;

        match next_seq {
            Some(expected) => {
                // In-order check: is this the expected sequence number?
                let diff = pkt.tcp_seq.wrapping_sub(*expected) as i32;
                if diff == 0 {
                    // In order — append.
                    buf.extend_from_slice(&pkt.payload);
                    *expected = pkt.tcp_seq.wrapping_add(pkt.payload.len() as u32);
                } else if diff < 0 {
                    // Retransmission or overlap — check if it extends past expected.
                    let overlap = (-diff) as usize;
                    if overlap < pkt.payload.len() {
                        buf.extend_from_slice(&pkt.payload[overlap..]);
                        *expected = pkt.tcp_seq.wrapping_add(pkt.payload.len() as u32);
                    }
                    // else: pure retransmission, ignore.
                }
                // else: out-of-order (diff > 0), drop for now.
            }
            None => {
                // No expected seq yet (mid-stream capture). Accept and start tracking.
                buf.extend_from_slice(&pkt.payload);
                *next_seq = Some(pkt.tcp_seq.wrapping_add(pkt.payload.len() as u32));
            }
        }
    }

    fn try_parse_http(&mut self, output: &mut Vec<HttpParseEvent>) -> bool {
        // Determine which buffer is client and which is server.
        let (
            client_buf,
            server_buf,
            client_addr,
            server_addr,
            client_ts,
            server_ts,
            server_last_ts,
        ) = match self.client_side {
            ClientSide::AtoB => (
                &mut self.a_to_b_buf,
                &mut self.b_to_a_buf,
                self.addr_a,
                self.addr_b,
                self.last_a_to_b_data_ts,
                self.last_b_to_a_data_ts,
                self.last_b_to_a_data_ts,
            ),
            ClientSide::BtoA => (
                &mut self.b_to_a_buf,
                &mut self.a_to_b_buf,
                self.addr_b,
                self.addr_a,
                self.last_b_to_a_data_ts,
                self.last_a_to_b_data_ts,
                self.last_a_to_b_data_ts,
            ),
            ClientSide::Unknown => return false,
        };

        let result = self.http_parser.parse(
            client_buf,
            server_buf,
            &self.flow_key,
            client_addr,
            server_addr,
            client_ts,
            server_ts,
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
            return true;
        }
        false
    }

    fn finish_pending_response(&mut self, output: &mut Vec<HttpParseEvent>) {
        let (server_buf, client_addr, server_addr, server_last_ts) = match self.client_side {
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

    pub fn is_closed(&self) -> bool {
        self.state == TcpState::Closed
    }

    /// Timestamp (µs) of the last packet received on this flow.
    pub fn last_pkt_ts(&self) -> i64 {
        self.last_pkt_ts
    }
}

/// Check if a buffer starts with an HTTP request method.
pub(crate) fn looks_like_http_request(buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    buf.starts_with(b"GET ")
        || buf.starts_with(b"POST ")
        || buf.starts_with(b"PUT ")
        || buf.starts_with(b"DELETE ")
        || buf.starts_with(b"PATCH ")
        || buf.starts_with(b"HEAD ")
        || buf.starts_with(b"OPTIONS ")
        || buf.starts_with(b"CONNECT ")
}

/// How often to scan for timed-out flows (30 seconds in µs).
const CLEANUP_INTERVAL_US: i64 = 30_000_000;

/// A flow with no packets for this duration is considered dead (120 seconds in µs).
const FLOW_TIMEOUT_US: i64 = 120_000_000;

/// A worker that processes packets for a set of flows (flow table).
///
/// Pure processor: holds no channel. Callers drive `process(input)` and
/// dispatch the returned `HttpParseEvent`s themselves — the spawn loop in
/// `ts_protocol::stage` owns the downstream `Sender`, so send failures are
/// observable at that single point.
pub struct FlowWorker {
    flows: HashMap<FlowKey, TcpFlow>,
    metrics: MetricsWorker,
    /// Per-source event-time of the last cleanup sweep (µs). Keyed by
    /// `source_id` so a fast-clock source's trigger cannot force a sweep —
    /// or evict flows — in a slow-clock source.
    last_cleanup_by_source: HashMap<String, i64>,
}

impl FlowWorker {
    pub fn new(metrics: MetricsWorker) -> Self {
        Self {
            flows: HashMap::new(),
            metrics,
            last_cleanup_by_source: HashMap::new(),
        }
    }

    /// Process a single worker input (packet or heartbeat) and return any
    /// downstream events produced. Pure in-memory work — no IO.
    pub fn process(&mut self, input: WorkerInput) -> Vec<HttpParseEvent> {
        let mut out = Vec::new();
        match input {
            WorkerInput::Packet(pkt) => self.process_packet(pkt, &mut out),
            WorkerInput::Heartbeat { ts, source_id } => {
                self.process_heartbeat(ts, source_id, &mut out)
            }
        }
        out
    }

    fn process_packet(&mut self, pkt: ParsedPacket, out: &mut Vec<HttpParseEvent>) {
        self.metrics.counter(Metric::NetPacketsParsed).inc();

        let flow_key = pkt.flow_key.clone();
        let flow = self
            .flows
            .entry(flow_key.clone())
            .or_insert_with(|| TcpFlow::new(flow_key.clone()));

        let start = out.len();
        let resync = flow.push(&pkt, out);
        if resync {
            self.metrics.counter(Metric::HttpResyncEvents).inc();
        }

        for event in &out[start..] {
            match event {
                HttpParseEvent::HttpRequest(_) => {
                    self.metrics.counter(Metric::HttpRequestsParsed).inc();
                }
                HttpParseEvent::HttpResponse(_) => {
                    self.metrics.counter(Metric::HttpResponsesParsed).inc();
                }
                HttpParseEvent::SseEvent(_) => {
                    self.metrics.counter(Metric::SseEventsParsed).inc();
                }
                HttpParseEvent::Heartbeat { .. } => {}
            }
        }

        // Clean up closed flows.
        if flow.is_closed() {
            self.flows.remove(&flow_key);
        }

        // Periodic timeout cleanup driven by packet timestamps.
        self.maybe_cleanup_stale_flows(&flow_key.source_id, pkt.timestamp_us, out);
    }

    /// Advance event time using an upstream heartbeat. Drives stale-flow
    /// cleanup during idle traffic, and emits a heartbeat event so later
    /// stages (llm, turn, metrics) can make their own progress.
    fn process_heartbeat(
        &mut self,
        wall_ts_us: i64,
        source_id: String,
        out: &mut Vec<HttpParseEvent>,
    ) {
        self.maybe_cleanup_stale_flows(&source_id, wall_ts_us, out);
        out.push(HttpParseEvent::Heartbeat {
            ts: wall_ts_us,
            source_id,
        });
    }

    /// Remove flows on `source_id` that have not received any packet within
    /// `FLOW_TIMEOUT_US`. Only runs when at least `CLEANUP_INTERVAL_US` has
    /// elapsed (by that source's event time) since its own last sweep. Flows
    /// on other sources are never inspected — their clocks advance on their
    /// own triggers.
    fn maybe_cleanup_stale_flows(
        &mut self,
        source_id: &str,
        now_ts: i64,
        out: &mut Vec<HttpParseEvent>,
    ) {
        let last = self
            .last_cleanup_by_source
            .entry(source_id.to_string())
            .or_insert(0);
        if now_ts - *last < CLEANUP_INTERVAL_US {
            return;
        }
        *last = now_ts;

        let timed_out_keys: Vec<FlowKey> = self
            .flows
            .iter()
            .filter(|(key, flow)| {
                key.source_id == source_id && now_ts - flow.last_pkt_ts() > FLOW_TIMEOUT_US
            })
            .map(|(k, _)| k.clone())
            .collect();

        if timed_out_keys.is_empty() {
            return;
        }

        tracing::trace!(count = timed_out_keys.len(), "cleaning up timed-out flows");

        for key in &timed_out_keys {
            if let Some(mut flow) = self.flows.remove(key) {
                flow.finish_pending_response(out);
            }
            self.metrics.counter(Metric::FlowsTimedOut).inc();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HttpParseEvent;
    use crate::net::{Direction, FlowKey, ParsedPacket, TCP_ACK, TCP_SYN};
    use bytes::Bytes;

    fn make_pkt(
        flow_key: &FlowKey,
        direction: Direction,
        payload: &[u8],
        seq: u32,
        tcp_flags: u8,
    ) -> ParsedPacket {
        let (src_ip, src_port, dst_ip, dst_port) = match direction {
            Direction::AtoB => (
                flow_key.addr_a.0,
                flow_key.addr_a.1,
                flow_key.addr_b.0,
                flow_key.addr_b.1,
            ),
            Direction::BtoA => (
                flow_key.addr_b.0,
                flow_key.addr_b.1,
                flow_key.addr_a.0,
                flow_key.addr_a.1,
            ),
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
            String::new(),
            "10.0.0.1".parse().unwrap(),
            5000,
            "10.0.0.2".parse().unwrap(),
            8080,
        )
    }

    #[test]
    fn test_mid_stream_join_discards_server_data_then_syncs() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        // First packet is server-direction response data — should be discarded.
        let resp_data = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        flow.push(
            &make_pkt(&fk, Direction::BtoA, resp_data, 1000, 0),
            &mut output,
        );
        assert!(
            output.is_empty(),
            "server data before sync should be discarded"
        );

        // Client sends a valid request — should sync and parse.
        let req = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";
        flow.push(&make_pkt(&fk, Direction::AtoB, req, 100, 0), &mut output);
        assert_eq!(
            output
                .iter()
                .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
                .count(),
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
        flow.push(
            &make_pkt(&fk, Direction::AtoB, &[], 0, TCP_SYN),
            &mut output,
        );
        flow.push(
            &make_pkt(&fk, Direction::BtoA, &[], 0, TCP_SYN | TCP_ACK),
            &mut output,
        );

        // First request.
        let req1 = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req1, 1, 0), &mut output);
        assert_eq!(output.len(), 1); // HttpRequest

        // No response arrives. Client sends a new request.
        let req2 = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req2, 100, 0), &mut output);

        // Second request should trigger resync and be parsed.
        let req_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
            .count();
        assert_eq!(req_count, 2, "second request should be parsed after resync");
    }

    #[test]
    fn test_http_parse_error_triggers_resync_then_recovers() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        // SYN.
        flow.push(
            &make_pkt(&fk, Direction::AtoB, &[], 0, TCP_SYN),
            &mut output,
        );
        flow.push(
            &make_pkt(&fk, Direction::BtoA, &[], 0, TCP_SYN | TCP_ACK),
            &mut output,
        );

        // Valid request.
        let req = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req, 1, 0), &mut output);
        assert_eq!(output.len(), 1);

        // Corrupt response (will cause NeedResync from HttpParser).
        let corrupt = b"\x00\x01\x02\r\n\r\n";
        flow.push(
            &make_pkt(&fk, Direction::BtoA, corrupt, 1000, 0),
            &mut output,
        );

        // Random server data — still unsynced, discarded.
        flow.push(
            &make_pkt(&fk, Direction::BtoA, b"more junk", 1100, 0),
            &mut output,
        );

        // New valid request — should resync and parse.
        let req2 = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";
        flow.push(&make_pkt(&fk, Direction::AtoB, req2, 200, 0), &mut output);
        let req_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
            .count();
        assert_eq!(req_count, 2, "should recover after resync");
    }

    fn make_pkt_ts(
        flow_key: &FlowKey,
        direction: Direction,
        payload: &[u8],
        seq: u32,
        tcp_flags: u8,
        timestamp_us: i64,
    ) -> ParsedPacket {
        let mut pkt = make_pkt(flow_key, direction, payload, seq, tcp_flags);
        pkt.timestamp_us = timestamp_us;
        pkt
    }

    #[test]
    fn test_last_pkt_ts_updated_on_push() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();
        assert_eq!(flow.last_pkt_ts(), 0);

        let req = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";
        flow.push(
            &make_pkt_ts(&fk, Direction::AtoB, req, 100, 0, 1_000_000),
            &mut output,
        );
        assert_eq!(flow.last_pkt_ts(), 1_000_000);

        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        flow.push(
            &make_pkt_ts(&fk, Direction::BtoA, resp, 200, 0, 2_000_000),
            &mut output,
        );
        assert_eq!(flow.last_pkt_ts(), 2_000_000);
    }

    fn new_test_worker() -> (FlowWorker, ts_common::internal_metrics::MetricsWorker) {
        use ts_common::internal_metrics::MetricsSystem;
        let mut sys = MetricsSystem::new();
        let metrics = sys.register_worker(
            "test",
            &[
                Metric::NetPacketsParsed,
                Metric::HttpRequestsParsed,
                Metric::HttpResponsesParsed,
                Metric::SseEventsParsed,
                Metric::HttpResyncEvents,
                Metric::FlowsTimedOut,
            ],
        );
        // start() just finalizes the registry; handles already hold their own Arcs.
        let _ = sys.start();
        (FlowWorker::new(metrics.clone()), metrics)
    }

    #[test]
    fn test_flow_timeout_cleanup() {
        let (mut worker, metrics) = new_test_worker();

        let fk = test_flow_key();
        let req = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";

        // T=0: first request
        let pkt = make_pkt_ts(&fk, Direction::AtoB, req, 100, 0, 0);
        let _ = worker.process(WorkerInput::Packet(pkt));
        assert_eq!(worker.flows.len(), 1, "flow should exist");

        // T=31s: trigger cleanup scan but flow is still within timeout (31s < 120s).
        let pkt = make_pkt_ts(&fk, Direction::AtoB, &[], 200, 0, 31_000_000);
        let _ = worker.process(WorkerInput::Packet(pkt));
        assert_eq!(
            worker.flows.len(),
            1,
            "flow should survive — not timed out yet"
        );

        // Create a second flow at T=31s.
        let fk2 = FlowKey::new(
            String::new(),
            "10.0.0.1".parse().unwrap(),
            6000,
            "10.0.0.2".parse().unwrap(),
            8080,
        );
        let pkt2 = make_pkt_ts(&fk2, Direction::AtoB, req, 100, 0, 31_000_000);
        let _ = worker.process(WorkerInput::Packet(pkt2));
        assert_eq!(worker.flows.len(), 2, "two flows should exist");

        let fk3 = FlowKey::new(
            String::new(),
            "10.0.0.1".parse().unwrap(),
            7000,
            "10.0.0.2".parse().unwrap(),
            8080,
        );
        let pkt3 = make_pkt_ts(&fk3, Direction::AtoB, req, 100, 0, 152_000_000);
        let _ = worker.process(WorkerInput::Packet(pkt3));
        // fk1 and fk2 should be cleaned, fk3 should remain.
        assert_eq!(worker.flows.len(), 1, "stale flows should be removed");
        assert!(worker.flows.contains_key(&fk3), "new flow should remain");
        assert_eq!(metrics.counter(Metric::FlowsTimedOut).get(), 2);
    }

    #[test]
    fn test_active_flow_survives_cleanup() {
        let (mut worker, metrics) = new_test_worker();

        let fk = test_flow_key();
        let req = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";

        // T=0: create flow.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            req,
            100,
            0,
            0,
        )));

        // T=100s: keep flow alive.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            &[],
            200,
            0,
            100_000_000,
        )));

        // T=131s: trigger cleanup. Flow was active at T=100s, only 31s ago — survives.
        let fk2 = FlowKey::new(
            String::new(),
            "10.0.0.1".parse().unwrap(),
            9000,
            "10.0.0.2".parse().unwrap(),
            8080,
        );
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk2,
            Direction::AtoB,
            req,
            100,
            0,
            131_000_000,
        )));
        assert!(
            worker.flows.contains_key(&fk),
            "active flow should survive cleanup"
        );
        assert_eq!(metrics.counter(Metric::FlowsTimedOut).get(), 0);
    }

    #[test]
    fn test_cleanup_is_per_source() {
        // Two flows on different sources. source-a advances past the 120s
        // timeout via heartbeat; source-b has never seen a new event, so its
        // flow must survive even though the wall age since creation exceeds
        // the timeout.
        let (mut worker, metrics) = new_test_worker();
        let req = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";

        let fk_a = FlowKey::new(
            "source-a".into(),
            "10.0.0.1".parse().unwrap(),
            5000,
            "10.0.0.2".parse().unwrap(),
            8080,
        );
        let fk_b = FlowKey::new(
            "source-b".into(),
            "10.0.0.1".parse().unwrap(),
            5000,
            "10.0.0.2".parse().unwrap(),
            8080,
        );

        // Both flows created at T=0 on their own source.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk_a,
            Direction::AtoB,
            req,
            100,
            0,
            0,
        )));
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk_b,
            Direction::AtoB,
            req,
            100,
            0,
            0,
        )));
        assert_eq!(worker.flows.len(), 2);

        // Heartbeat on source-a at T=200s. Only source-a's flow should be
        // evicted; source-b's flow has no trigger on its own clock yet.
        let _ = worker.process(WorkerInput::Heartbeat {
            ts: 200_000_000,
            source_id: "source-a".into(),
        });
        assert!(
            worker.flows.contains_key(&fk_b),
            "source-b flow must survive a foreign source's heartbeat"
        );
        assert!(
            !worker.flows.contains_key(&fk_a),
            "source-a flow must be evicted by its own source's heartbeat"
        );
        assert_eq!(metrics.counter(Metric::FlowsTimedOut).get(), 1);
    }

    #[test]
    fn test_syn_handshake_sets_synced() {
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        flow.push(
            &make_pkt(&fk, Direction::AtoB, &[], 0, TCP_SYN),
            &mut output,
        );
        flow.push(
            &make_pkt(&fk, Direction::BtoA, &[], 0, TCP_SYN | TCP_ACK),
            &mut output,
        );

        // Data should be accepted without needing looks_like_http_request.
        let req = b"POST /v1/chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        flow.push(&make_pkt(&fk, Direction::AtoB, req, 1, 0), &mut output);
        assert_eq!(output.len(), 1);
    }
}
