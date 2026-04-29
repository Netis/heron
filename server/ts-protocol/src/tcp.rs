use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;

use bytes::{Bytes, BytesMut};

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

/// Per-direction reassembly state. Keeping these together makes
/// direction-selection in `append_payload` / `try_parse_http` /
/// `finish_pending_response` a single `&mut self.{a_to_b,b_to_a}` borrow,
/// and gives every future per-direction concept (SACK ranges, OOO buffer)
/// a natural home.
struct DirState {
    /// Reassembled in-order bytes pending HTTP parse.
    buf: BytesMut,
    /// Next expected sequence number; `None` until we observe SYN or the
    /// first data packet (mid-stream sync).
    next_seq: Option<u32>,
    /// Timestamp of the most recent data packet in this direction. Used as
    /// the stamping time for HTTP requests/responses at parse time — on
    /// keep-alive connections the connection's first-packet time is not a
    /// valid per-request start time.
    last_data_ts: i64,
    /// Out-of-order segment buffer. Lazily allocated: stays `None` for any
    /// flow that never sees an OOO arrival, so the in-order hot path costs
    /// one `Option` discriminant test. Capped at `OOO_CAP_SEGMENTS`; when
    /// full we evict the lowest-seq entry. Live span is bounded by the cap
    /// (≪ 2³¹ bytes), so `BTreeMap`'s natural u32 ordering matches the
    /// wraparound-aware logical ordering used in `drain_ooo`.
    ooo: Option<BTreeMap<u32, Bytes>>,
}

impl DirState {
    fn new() -> Self {
        Self {
            buf: BytesMut::new(),
            next_seq: None,
            last_data_ts: 0,
            ooo: None,
        }
    }

    /// Discard all in-flight reassembly state on this direction: the
    /// in-order buffer and any segments held in the OOO map. Used by every
    /// resync site so a stale gap can't leak across a flow restart.
    /// `next_seq` and `last_data_ts` are not touched here — callers manage
    /// them per resync semantics.
    fn discard_buffers(&mut self) {
        self.buf.clear();
        self.ooo = None;
    }
}

/// Per-flow TCP state and reassembly buffers.
pub struct TcpFlow {
    state: TcpState,
    client_side: ClientSide,
    /// Whether we are synchronized with the HTTP stream.
    /// `synced == true` implies `client_side != Unknown`.
    synced: bool,

    /// Per-direction reassembly state.
    a_to_b: DirState,
    b_to_a: DirState,

    /// Timestamp of the last packet received on this flow (any direction).
    last_pkt_ts: i64,

    /// Pending counters drained by FlowWorker into shared metrics after each
    /// `push`. We keep the raw bumps inside `append_payload` (where the
    /// branches live) and let the caller hand them off to the per-shard
    /// `MetricsWorker` once per packet — TcpFlow itself stays IO-free.
    ooo_drops_pending: u32,
    ooo_buffered_pending: u32,
    rexmit_ignored_pending: u32,

    // HTTP parser operates on the reassembled buffers.
    http_parser: HttpParser,

    // Connection identity for events.
    flow_key: FlowKey,
    addr_a: (IpAddr, u16),
    addr_b: (IpAddr, u16),
}

impl TcpFlow {
    /// Per-direction state mutable access. Centralizing this here keeps
    /// callers free of `match pkt.direction { ... }` boilerplate.
    fn dir_mut(&mut self, d: Direction) -> &mut DirState {
        match d {
            Direction::AtoB => &mut self.a_to_b,
            Direction::BtoA => &mut self.b_to_a,
        }
    }

    pub fn new(flow_key: FlowKey) -> Self {
        let addr_a = flow_key.addr_a;
        let addr_b = flow_key.addr_b;
        Self {
            state: TcpState::Init,
            client_side: ClientSide::Unknown,
            synced: false,
            a_to_b: DirState::new(),
            b_to_a: DirState::new(),
            last_pkt_ts: 0,
            ooo_drops_pending: 0,
            ooo_buffered_pending: 0,
            rexmit_ignored_pending: 0,
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
            self.dir_mut(pkt.direction).next_seq = Some(pkt.tcp_seq.wrapping_add(1));
            return false;
        }

        // SYN-ACK.
        if pkt.has_syn() && pkt.has_ack() {
            self.state = TcpState::Established;
            self.dir_mut(pkt.direction).next_seq = Some(pkt.tcp_seq.wrapping_add(1));
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

        // Snaplen truncation: the on-wire segment carried more bytes than the
        // capture preserved. Splicing the captured bytes into `dir.buf` while
        // advancing `next_seq` by the wire length would leave a phantom gap
        // (Content-Length bodies hang at NeedMore forever; chunked decoders
        // mis-frame the next chunk). Discard everything on this flow and let
        // the next intact request re-sync via the existing mid-stream path.
        // Bounded loss: the truncated call only. Same cleanup shape as the
        // `NeedResync` site below — `client_side` is preserved and overwritten
        // by the next sync.
        if (pkt.wire_payload_len as usize) > pkt.payload.len() {
            tracing::trace!(
                flow = %self.flow_key,
                wire_len = pkt.wire_payload_len,
                cap_len = pkt.payload.len(),
                "resync: snaplen-truncated segment"
            );
            self.synced = false;
            self.a_to_b.discard_buffers();
            self.b_to_a.discard_buffers();
            self.http_parser.reset();
            self.a_to_b.next_seq = None;
            self.b_to_a.next_seq = None;
            return true;
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
                    self.a_to_b.discard_buffers();
                    self.b_to_a.discard_buffers();
                    self.http_parser.reset();
                    // Drop stale per-direction seq tracking — the pkt we're
                    // syncing on becomes the new baseline via append_payload's
                    // mid-stream branch. Without this, post-resync packets
                    // get diff'd against a stale `expected` and may end up in
                    // the OOO buffer instead of the in-order buf.
                    self.a_to_b.next_seq = None;
                    self.b_to_a.next_seq = None;
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
                    self.a_to_b.discard_buffers();
                    self.b_to_a.discard_buffers();
                    self.http_parser.reset();
                    self.a_to_b.next_seq = None;
                    self.b_to_a.next_seq = None;
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
        // Direct match (not `dir_mut`) so the borrow checker can split:
        // the closure borrows only `a_to_b` or `b_to_a`, leaving the pending
        // counter fields freely mutable in the OOO branch below.
        let dir = match pkt.direction {
            Direction::AtoB => &mut self.a_to_b,
            Direction::BtoA => &mut self.b_to_a,
        };
        dir.last_data_ts = pkt.timestamp_us;

        let Some(expected) = dir.next_seq.as_mut() else {
            // No expected seq yet (mid-stream capture). Accept and start tracking.
            // The OOO buffer cannot have entries yet because nothing was ever
            // tracked relative to an expected seq.
            dir.buf.extend_from_slice(&pkt.payload);
            dir.next_seq = Some(pkt.tcp_seq.wrapping_add(pkt.wire_payload_len));
            return;
        };

        // Seq math runs on the on-wire segment length (same as `payload.len()`
        // for intact packets — `push` filters truncated packets out earlier).
        let wire_len = pkt.wire_payload_len;
        let diff = pkt.tcp_seq.wrapping_sub(*expected) as i32;
        if diff == 0 {
            // Hot path — in-order. Append, advance, then drain any OOO segments
            // that just became contiguous. The `is_some_and` check is one
            // discriminant test on the lazily-allocated map.
            dir.buf.extend_from_slice(&pkt.payload);
            *expected = pkt.tcp_seq.wrapping_add(wire_len);
            if dir.ooo.as_ref().is_some_and(|m| !m.is_empty()) {
                drain_ooo(dir);
            }
        } else if diff < 0 {
            // Retransmission or overlap — check if it extends past expected.
            let overlap = (-diff) as usize;
            if overlap < pkt.payload.len() {
                dir.buf.extend_from_slice(&pkt.payload[overlap..]);
                *expected = pkt.tcp_seq.wrapping_add(wire_len);
                if dir.ooo.as_ref().is_some_and(|m| !m.is_empty()) {
                    drain_ooo(dir);
                }
            } else if !pkt.payload.is_empty() {
                // Pure retransmission of already-buffered bytes.
                self.rexmit_ignored_pending = self.rexmit_ignored_pending.saturating_add(1);
            }
        } else if !pkt.payload.is_empty() {
            // Out-of-order (diff > 0): segment ahead of `expected`. Buffer it;
            // a future in-order arrival will drain it back into `buf`.
            let ooo = dir.ooo.get_or_insert_with(BTreeMap::new);
            if ooo.len() >= OOO_CAP_SEGMENTS {
                // Evict oldest (lowest seq). It is the segment furthest from
                // the current `expected`, so most likely stranded; the freshly
                // arrived segment is closer to the next in-order arrival.
                if let Some(oldest) = ooo.keys().next().copied() {
                    ooo.remove(&oldest);
                }
                self.ooo_drops_pending = self.ooo_drops_pending.saturating_add(1);
            }
            // `pkt.payload` is already a `Bytes` view; insert is a refcount bump.
            ooo.insert(pkt.tcp_seq, pkt.payload.clone());
            self.ooo_buffered_pending = self.ooo_buffered_pending.saturating_add(1);
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
                &mut self.a_to_b.buf,
                &mut self.b_to_a.buf,
                self.addr_a,
                self.addr_b,
                self.a_to_b.last_data_ts,
                self.b_to_a.last_data_ts,
                self.b_to_a.last_data_ts,
            ),
            ClientSide::BtoA => (
                &mut self.b_to_a.buf,
                &mut self.a_to_b.buf,
                self.addr_b,
                self.addr_a,
                self.b_to_a.last_data_ts,
                self.a_to_b.last_data_ts,
                self.a_to_b.last_data_ts,
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
            self.a_to_b.discard_buffers();
            self.b_to_a.discard_buffers();
            self.http_parser.reset();
            // Same rationale as the unsynced→synced site: drop seq tracking
            // so the next packet on each direction re-baselines.
            self.a_to_b.next_seq = None;
            self.b_to_a.next_seq = None;
            return true;
        }
        false
    }

    fn finish_pending_response(&mut self, output: &mut Vec<HttpParseEvent>) {
        let (server_buf, client_addr, server_addr, server_last_ts) = match self.client_side {
            ClientSide::AtoB => (
                &mut self.b_to_a.buf,
                self.addr_a,
                self.addr_b,
                self.b_to_a.last_data_ts,
            ),
            ClientSide::BtoA => (
                &mut self.a_to_b.buf,
                self.addr_b,
                self.addr_a,
                self.a_to_b.last_data_ts,
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

    /// Drain the reassembler-branch counters accumulated since the last call.
    /// Returned tuple: `(ooo_drops, rexmit_ignored)`.
    pub fn take_append_stats(&mut self) -> (u32, u32, u32) {
        let drops = std::mem::take(&mut self.ooo_drops_pending);
        let buffered = std::mem::take(&mut self.ooo_buffered_pending);
        let rex = std::mem::take(&mut self.rexmit_ignored_pending);
        (drops, buffered, rex)
    }
}

/// Drain consecutive OOO segments from `dir.ooo` into `dir.buf` starting at
/// `dir.next_seq`. Stops at the first remaining gap. Handles partial overlap
/// (a buffered segment whose head is already covered by an in-order append).
///
/// Caller must guarantee `dir.next_seq.is_some()` and `dir.ooo.is_some()`.
fn drain_ooo(dir: &mut DirState) {
    let Some(ooo) = dir.ooo.as_mut() else { return };
    let expected = dir
        .next_seq
        .as_mut()
        .expect("drain_ooo requires next_seq to be set");

    while let Some((&seq, _)) = ooo.iter().next() {
        let seq_diff = seq.wrapping_sub(*expected) as i32;
        if seq_diff > 0 {
            // First entry still has a gap before it — stop.
            break;
        }
        let bytes = ooo.remove(&seq).expect("just peeked");
        if seq_diff == 0 {
            dir.buf.extend_from_slice(&bytes);
            *expected = seq.wrapping_add(bytes.len() as u32);
        } else {
            // seq_diff < 0: buffered segment's head is already covered.
            let behind = (-seq_diff) as usize;
            if behind < bytes.len() {
                dir.buf.extend_from_slice(&bytes[behind..]);
                *expected = seq.wrapping_add(bytes.len() as u32);
            }
            // else: entirely behind `expected` — stale, drop silently. This
            // entry was already counted at insert time; skipping it here is
            // not a separate event.
        }
    }

    // Free the map allocation when empty so the next idle period costs zero.
    if ooo.is_empty() {
        dir.ooo = None;
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

/// Maximum out-of-order segments held per direction before eviction kicks in.
/// Worst-case memory ≈ 32 × MSS ≈ 48 KB / direction = 96 KB / flow. The cap is
/// also an implicit guarantee that the buffered window stays well under 2³¹
/// bytes, so `BTreeMap`'s natural u32 ordering matches wraparound-aware logical
/// ordering used in `drain_ooo`.
const OOO_CAP_SEGMENTS: usize = 32;

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

    /// Public so the stage loop can sample live-flow count into a
    /// per-shard atomic gauge for the `flows_active` metric.
    pub fn flow_count(&self) -> usize {
        self.flows.len()
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
        let (ooo_drops, ooo_buffered, rex) = flow.take_append_stats();
        if ooo_drops > 0 {
            self.metrics
                .counter(Metric::TcpOutOfOrderDrops)
                .add(ooo_drops as u64);
        }
        if ooo_buffered > 0 {
            self.metrics
                .counter(Metric::TcpOutOfOrderBuffered)
                .add(ooo_buffered as u64);
        }
        if rex > 0 {
            self.metrics
                .counter(Metric::TcpRetransmissionsIgnored)
                .add(rex as u64);
        }

        for event in &out[start..] {
            match event {
                HttpParseEvent::HttpRequest(_) => {
                    self.metrics.counter(Metric::HttpParseReq).inc();
                }
                HttpParseEvent::HttpResponse(_) => {
                    self.metrics.counter(Metric::HttpParseResp).inc();
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
        self.metrics.counter(Metric::FlowHeartbeatsReceived).inc();
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
            self.metrics.counter(Metric::FlowsExpired).inc();
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
            wire_payload_len: payload.len() as u32,
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
    fn test_needresync_clears_next_seq_so_unsynced_resync_recovers() {
        // Without the next_seq=None reset on the NeedResync path, a fresh
        // request after parser failure ends up routed to the OOO buffer
        // (because diff>0 against the stale `expected`) and never reaches
        // the in-order buf. Then the unsynced-sync site's append_payload
        // also sees diff>0 and buffers it as OOO, parser sees nothing,
        // request is silently lost.
        //
        // With the consistency fix, NeedResync drops next_seq=None; the
        // following unsynced-sync packet enters the mid-stream branch and
        // re-baselines, so the request parses cleanly.
        let fk = test_flow_key();
        let mut flow = TcpFlow::new(fk.clone());
        let mut output = Vec::new();

        // SYN handshake: both directions get next_seq=Some(1).
        flow.push(
            &make_pkt(&fk, Direction::AtoB, &[], 0, TCP_SYN),
            &mut output,
        );
        flow.push(
            &make_pkt(&fk, Direction::BtoA, &[], 0, TCP_SYN | TCP_ACK),
            &mut output,
        );

        // First request — parsed normally; AtoB.next_seq advances past it.
        let req1 = b"POST /v1/x HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n";
        flow.push(&make_pkt(&fk, Direction::AtoB, req1, 1, 0), &mut output);
        assert_eq!(output.len(), 1);

        // Corrupt response *in-order* on BtoA so it actually reaches the
        // parser and trips NeedResync (an OOO seq would just be buffered).
        let corrupt = b"\x00\x01\x02 not http\r\n\r\n";
        flow.push(&make_pkt(&fk, Direction::BtoA, corrupt, 1, 0), &mut output);

        // Fresh request on AtoB at a *high* seq (simulating that some time
        // passed and seq has advanced). With the consistency fix, the
        // unsynced-sync site treats this as the new mid-stream baseline.
        let req2 = b"GET /v1/models HTTP/1.1\r\nHost: h\r\n\r\n";
        flow.push(
            &make_pkt(&fk, Direction::AtoB, req2, 50_000, 0),
            &mut output,
        );

        let req_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
            .count();
        assert_eq!(req_count, 2, "fresh request after NeedResync must parse");
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

    /// Build a packet whose wire segment was longer than what the capture
    /// preserved (snaplen truncation). Mirrors the lo.pcap repro where
    /// `cap_len < len` left tail bytes behind.
    fn make_truncated_pkt(
        flow_key: &FlowKey,
        direction: Direction,
        captured: &[u8],
        wire_len: u32,
        seq: u32,
    ) -> ParsedPacket {
        let mut pkt = make_pkt(flow_key, direction, captured, seq, 0);
        pkt.wire_payload_len = wire_len;
        pkt
    }

    #[test]
    fn test_truncated_segment_forces_resync_and_subsequent_request_parses() {
        // Reproduces the lo.pcap cascade: a keep-alive flow carries a long
        // POST whose body segment got snaplen-truncated, then a follow-up
        // request on the same flow. Without the truncation guard, every
        // subsequent in-order packet drifts into the OOO buffer (because
        // `next_seq` undershoots by the truncation amount) and the
        // follow-up request's HttpRequest is never emitted. With the guard,
        // the truncated segment forces a resync — the truncated POST is
        // intentionally lost (its body bytes are unrecoverable) but the
        // follow-up request mid-stream-syncs cleanly.
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

        // Client begins a POST whose body extends across multiple segments;
        // headers + first body bytes arrive intact.
        let head = b"POST /v1/x HTTP/1.1\r\nHost: h\r\nContent-Length: 200\r\n\r\nABCDEFGHIJ";
        flow.push(&make_pkt(&fk, Direction::AtoB, head, 1, 0), &mut output);
        assert_eq!(
            output
                .iter()
                .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
                .count(),
            0,
            "first request body still incomplete; no event yet"
        );

        // Continuation segment is snaplen-truncated: wire carried 100 bytes,
        // capture only preserved 80. Without the guard, `next_seq` would
        // advance by 80 instead of 100 and the next packet's seq would diff
        // by +20 → buffered as OOO and the parser starves.
        let captured = vec![b'X'; 80];
        let trunc_pkt =
            make_truncated_pkt(&fk, Direction::AtoB, &captured, 100, 1 + head.len() as u32);
        let resync = flow.push(&trunc_pkt, &mut output);
        assert!(
            resync,
            "truncated segment must force a resync (return true)"
        );

        // Follow-up keep-alive request on the same flow at the wire-correct
        // seq (continuing from the truncated segment's tail).
        let req2 = b"GET /v1/models HTTP/1.1\r\nHost: h\r\n\r\n";
        flow.push(
            &make_pkt(&fk, Direction::AtoB, req2, 1 + head.len() as u32 + 100, 0),
            &mut output,
        );

        let req_count = output
            .iter()
            .filter(|e| matches!(e, HttpParseEvent::HttpRequest(_)))
            .count();
        assert_eq!(
            req_count, 1,
            "follow-up request must mid-stream-sync after truncation resync"
        );
    }

    #[test]
    fn test_truncated_segment_clears_ooo_on_both_directions() {
        // Stronger structural assertion: after a truncated segment, *both*
        // directions' OOO maps must be empty and `next_seq` must be `None`,
        // so any pending stale state cannot leak across the resync.
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

        // Plant an OOO segment on each direction.
        flow.push(
            &make_pkt(&fk, Direction::AtoB, b"AHEAD-A", 5_000, 0),
            &mut output,
        );
        flow.push(
            &make_pkt(&fk, Direction::BtoA, b"AHEAD-B", 5_000, 0),
            &mut output,
        );
        assert!(flow.a_to_b.ooo.is_some());
        assert!(flow.b_to_a.ooo.is_some());

        // Truncated client→server segment: wire 50, captured 30.
        let captured = vec![b'Y'; 30];
        flow.push(
            &make_truncated_pkt(&fk, Direction::AtoB, &captured, 50, 1),
            &mut output,
        );

        assert!(
            flow.a_to_b.ooo.is_none(),
            "AtoB OOO must be cleared on truncation resync"
        );
        assert!(
            flow.b_to_a.ooo.is_none(),
            "BtoA OOO must be cleared on truncation resync"
        );
        assert_eq!(flow.a_to_b.next_seq, None);
        assert_eq!(flow.b_to_a.next_seq, None);
        assert!(!flow.synced);
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
                Metric::HttpParseReq,
                Metric::HttpParseResp,
                Metric::SseEventsParsed,
                Metric::HttpResyncEvents,
                Metric::TcpOutOfOrderDrops,
                Metric::TcpOutOfOrderBuffered,
                Metric::TcpRetransmissionsIgnored,
                Metric::FlowsExpired,
                Metric::FlowHeartbeatsReceived,
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
        assert_eq!(metrics.counter(Metric::FlowsExpired).get(), 2);
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
        assert_eq!(metrics.counter(Metric::FlowsExpired).get(), 0);
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
        assert_eq!(metrics.counter(Metric::FlowsExpired).get(), 1);
    }

    #[test]
    fn test_flow_count_tracks_flow_table() {
        let (mut worker, _) = new_test_worker();
        assert_eq!(worker.flow_count(), 0);

        let fk1 = test_flow_key();
        let req = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk1,
            Direction::AtoB,
            req,
            100,
            0,
            0,
        )));
        assert_eq!(worker.flow_count(), 1);

        let fk2 = FlowKey::new(
            String::new(),
            "10.0.0.1".parse().unwrap(),
            6000,
            "10.0.0.2".parse().unwrap(),
            8080,
        );
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk2,
            Direction::AtoB,
            req,
            100,
            0,
            1_000_000,
        )));
        assert_eq!(worker.flow_count(), 2);

        // Heartbeat past the 120s timeout evicts both flows.
        let _ = worker.process(WorkerInput::Heartbeat {
            ts: 200_000_000,
            source_id: String::new(),
        });
        assert_eq!(worker.flow_count(), 0);
    }

    /// Test-only helper: SYN/SYN-ACK handshake on the given flow worker so
    /// both directions have `next_seq=Some(1)`.
    fn handshake(worker: &mut FlowWorker, fk: &FlowKey) {
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            fk,
            Direction::AtoB,
            &[],
            0,
            TCP_SYN,
            0,
        )));
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            fk,
            Direction::BtoA,
            &[],
            0,
            TCP_SYN | TCP_ACK,
            0,
        )));
    }

    /// Read out a flow's reassembled AtoB buffer for assertion purposes.
    /// Test-only escape hatch — production code never inspects `buf` directly.
    fn flow_a_to_b_buf<'a>(worker: &'a FlowWorker, fk: &FlowKey) -> &'a [u8] {
        worker
            .flows
            .get(fk)
            .expect("flow must exist")
            .a_to_b
            .buf
            .as_ref()
    }

    fn flow_a_to_b_next_seq(worker: &FlowWorker, fk: &FlowKey) -> Option<u32> {
        worker
            .flows
            .get(fk)
            .expect("flow must exist")
            .a_to_b
            .next_seq
    }

    fn flow_a_to_b_ooo_smallest_seq(worker: &FlowWorker, fk: &FlowKey) -> Option<u32> {
        worker
            .flows
            .get(fk)
            .expect("flow must exist")
            .a_to_b
            .ooo
            .as_ref()
            .and_then(|m| m.keys().next().copied())
    }

    fn flow_a_to_b_ooo_is_none(worker: &FlowWorker, fk: &FlowKey) -> bool {
        worker
            .flows
            .get(fk)
            .expect("flow must exist")
            .a_to_b
            .ooo
            .is_none()
    }

    fn flow_b_to_a_ooo_is_none(worker: &FlowWorker, fk: &FlowKey) -> bool {
        worker
            .flows
            .get(fk)
            .expect("flow must exist")
            .b_to_a
            .ooo
            .is_none()
    }

    #[test]
    fn test_ooo_drop_and_retransmit_counters() {
        // After SYN handshake the flow has `next_seq=Some(1)`. A segment
        // arriving 10 bytes ahead of `expected` is now buffered (not dropped)
        // and bumps TcpOutOfOrderBuffered. A subsequent segment that re-sends
        // bytes already in the buffer bumps TcpRetransmissionsIgnored.
        let (mut worker, metrics) = new_test_worker();
        let fk = test_flow_key();
        handshake(&mut worker, &fk);

        // First request seq=1, 8 bytes — buffer advances to seq=9.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"GET / HT",
            1,
            0,
            1_000,
        )));

        // OOO: seq=21 (gap of 12) — buffered, not dropped.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"AHEAD",
            21,
            0,
            2_000,
        )));
        assert_eq!(metrics.counter(Metric::TcpOutOfOrderBuffered).get(), 1);
        assert_eq!(metrics.counter(Metric::TcpOutOfOrderDrops).get(), 0);

        // Pure retransmission of seq=1's 8-byte payload — overlap (8) >=
        // payload.len() (8), so it's ignored.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"GET / HT",
            1,
            0,
            3_000,
        )));
        assert_eq!(metrics.counter(Metric::TcpRetransmissionsIgnored).get(), 1);
    }

    #[test]
    fn test_ooo_three_segments_arriving_1_3_2_yields_merged_buffer() {
        // SYN → expected=1. Send seq=1/5B "AAAAA", seq=11/5B "CCCCC" (OOO),
        // seq=6/5B "BBBBB" (closes gap). After the in-order arrival of
        // "BBBBB", `drain_ooo` must pull the buffered "CCCCC" and produce
        // the contiguous "AAAAABBBBBCCCCC".
        let (mut worker, metrics) = new_test_worker();
        let fk = test_flow_key();
        handshake(&mut worker, &fk);

        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"AAAAA",
            1,
            0,
            1_000,
        )));
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"CCCCC",
            11,
            0,
            2_000,
        )));
        // After the OOO buffer step, expected is still 6 and the map holds
        // one entry at seq=11. Buffer is just "AAAAA".
        assert_eq!(flow_a_to_b_buf(&worker, &fk), b"AAAAA");
        assert_eq!(flow_a_to_b_next_seq(&worker, &fk), Some(6));

        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"BBBBB",
            6,
            0,
            3_000,
        )));

        assert_eq!(flow_a_to_b_buf(&worker, &fk), b"AAAAABBBBBCCCCC");
        assert_eq!(flow_a_to_b_next_seq(&worker, &fk), Some(16));
        assert_eq!(metrics.counter(Metric::TcpOutOfOrderBuffered).get(), 1);
        assert_eq!(metrics.counter(Metric::TcpOutOfOrderDrops).get(), 0);
    }

    #[test]
    fn test_ooo_cap_eviction_evicts_oldest_and_bumps_drop() {
        // Buffer 33 OOO segments at seqs 100, 200, ..., 3300 — gap to
        // expected=1 is huge, so all stay buffered. The 33rd insert must
        // hit the cap (32) and evict the lowest-seq entry (seq=100), bumping
        // TcpOutOfOrderDrops once. The smallest remaining seq is 200.
        let (mut worker, metrics) = new_test_worker();
        let fk = test_flow_key();
        handshake(&mut worker, &fk);

        for i in 0..OOO_CAP_SEGMENTS as u32 + 1 {
            let seq = 100 + i * 100;
            let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
                &fk,
                Direction::AtoB,
                b"X",
                seq,
                0,
                10_000 + i as i64,
            )));
        }

        assert_eq!(
            metrics.counter(Metric::TcpOutOfOrderBuffered).get(),
            (OOO_CAP_SEGMENTS as u64) + 1
        );
        assert_eq!(metrics.counter(Metric::TcpOutOfOrderDrops).get(), 1);
        assert_eq!(flow_a_to_b_ooo_smallest_seq(&worker, &fk), Some(200));
    }

    #[test]
    fn test_drain_handles_partially_overlapping_buffered_segment() {
        // SYN → expected=1. Buffer seq=50/20B (range [50..70)). Then send
        // an in-order seq=1/60B that extends expected to 61. The buffered
        // segment now overlaps: bytes [50..61) are stale, [61..70) extend
        // the in-order buffer. Drain must append exactly bytes[11..20] and
        // advance expected to 70. Final buffer contains the 60-byte payload
        // followed by the 9-byte tail.
        let (mut worker, metrics) = new_test_worker();
        let fk = test_flow_key();
        handshake(&mut worker, &fk);

        // 20-byte OOO segment at seq=50.
        let ooo_payload = b"OOOOOOOOOOTAILTAILXX";
        assert_eq!(ooo_payload.len(), 20);
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            ooo_payload,
            50,
            0,
            1_000,
        )));

        // 60-byte in-order segment at seq=1.
        let in_order = vec![b'A'; 60];
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            &in_order,
            1,
            0,
            2_000,
        )));

        let buf = flow_a_to_b_buf(&worker, &fk);
        assert_eq!(buf.len(), 69, "60 in-order + 9 tail of buffered");
        assert_eq!(&buf[..60], in_order.as_slice());
        assert_eq!(&buf[60..], &ooo_payload[11..]);
        assert_eq!(flow_a_to_b_next_seq(&worker, &fk), Some(70));
        assert_eq!(metrics.counter(Metric::TcpOutOfOrderBuffered).get(), 1);
        assert_eq!(metrics.counter(Metric::TcpOutOfOrderDrops).get(), 0);
        assert_eq!(metrics.counter(Metric::TcpRetransmissionsIgnored).get(), 0);
    }

    #[test]
    fn test_resync_clears_ooo_on_both_directions() {
        // Reproduces the bug fixed by `DirState::discard_buffers`. Establish
        // a flow, drive it to `is_waiting_for_response()`, queue an OOO
        // segment in *each* direction, then trigger the
        // "new request while waiting for response" resync. After resync the
        // OOO map on both directions must be released — otherwise stale
        // gap-bridging segments would corrupt the post-resync flow state.
        let (mut worker, _metrics) = new_test_worker();
        let fk = test_flow_key();
        handshake(&mut worker, &fk);

        // Client sends a complete request with Content-Length: 0 so the
        // parser advances to WaitingForResponse without needing body bytes.
        let req1 = b"POST /v1/x HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n";
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            req1,
            1,
            0,
            1_000,
        )));

        // Buffer an OOO segment on the server side (response gap simulated).
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::BtoA,
            b"GAP-AHEAD",
            500,
            0,
            2_000,
        )));
        // And one on the client side too — keep-alive client could enqueue
        // its next request body bytes ahead of the new request's first seg.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"AHEAD-A",
            1_000,
            0,
            3_000,
        )));
        assert!(!flow_a_to_b_ooo_is_none(&worker, &fk));
        assert!(!flow_b_to_a_ooo_is_none(&worker, &fk));

        // New POST while waiting for response → triggers the resync branch.
        let req2 = b"POST /v1/x HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n";
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            req2,
            req1.len() as u32 + 1,
            0,
            4_000,
        )));

        // Both directions' OOO maps must be released by the resync path.
        assert!(
            flow_a_to_b_ooo_is_none(&worker, &fk),
            "AtoB OOO must be cleared on resync"
        );
        assert!(
            flow_b_to_a_ooo_is_none(&worker, &fk),
            "BtoA OOO must be cleared on resync"
        );
    }

    #[test]
    fn test_ooo_buffer_freed_when_drained_empty() {
        // After a drain that empties the OOO map, `dir.ooo` must return to
        // `None` so that the flow's idle memory cost is zero.
        let (mut worker, _metrics) = new_test_worker();
        let fk = test_flow_key();
        handshake(&mut worker, &fk);

        // seq=1 fills first 5 bytes; seq=11 buffered; seq=6 closes the gap
        // and drains seq=11. Map should be released.
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"AAAAA",
            1,
            0,
            1_000,
        )));
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"CCCCC",
            11,
            0,
            2_000,
        )));
        let _ = worker.process(WorkerInput::Packet(make_pkt_ts(
            &fk,
            Direction::AtoB,
            b"BBBBB",
            6,
            0,
            3_000,
        )));

        assert!(flow_a_to_b_ooo_is_none(&worker, &fk));
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
