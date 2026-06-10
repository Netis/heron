//! Stream-to-packet frame synthesis.
//!
//! Converts per-connection plaintext byte chunks — as produced by an eBPF
//! `SSL_read` / `SSL_write` uprobe — into synthetic Ethernet + IPv4/IPv6 + TCP
//! [`RawPacket`]s that feed Heron's existing dispatcher → TCP reassembler →
//! HTTP parser pipeline unchanged. No new ingress layer is needed: the
//! reassembler already handles mid-stream sync, multi-segment bodies, and
//! connection teardown, so an eBPF source only has to dress its plaintext
//! chunks as well-formed TCP segments.
//!
//! This module is **pure and cross-platform**: it has no eBPF dependency and
//! compiles/tests on any host. The Linux-only `EbpfSource` (Phase 1) drives it
//! from ring-buffer events, but the synthesis logic — the one genuinely novel
//! correctness surface — is validated here and in `h-protocol`'s integration
//! tests without a kernel.
//!
//! # Invariants the reassembler imposes (see `h-protocol`)
//!
//! * **IP length must cover exactly the payload present.** `de::decode` derives
//!   the on-wire segment length from the IP/TCP header fields; if it claims more
//!   bytes than are captured, `tcp.rs`'s truncation guard discards the flow.
//!   Every frame here sets the IP length to exactly `headers + payload`, so
//!   `wire_payload_len == payload.len()` always.
//! * **No checksums are validated** (`de/l3.rs`, `de/l4.rs` never read them), so
//!   IP/TCP checksum fields are left zero.
//! * **Sequence numbers advance monotonically per direction** by the emitted
//!   payload length. There are no retransmits or out-of-order frames — the
//!   plaintext stream is already in order.
//! * **Heartbeat sentinels are distinct.** Synthetic frames carry a real IPv4 /
//!   IPv6 ethertype, never `0xFFFF`, so [`RawPacket::is_heartbeat`] is false.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use bytes::Bytes;

use crate::packet::RawPacket;

// Wire constants. Mirrored locally (same pattern as `cloud_probe.rs`) because
// `h-capture` must not depend on `h-protocol`, where the canonical copies live.
const LINKTYPE_ETHERNET: u32 = 1;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_IPV6: u16 = 0x86DD;
const IP_PROTO_TCP: u8 = 6;

const ETH_HDR_LEN: usize = 14;
const IPV4_HDR_LEN: usize = 20;
const IPV6_HDR_LEN: usize = 40;
const TCP_HDR_LEN: usize = 20;

// TCP flags.
const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;

/// Largest TCP payload a single synthesized segment may carry. The IPv4
/// `total_length` (and IPv6 `payload_length`) field is 16-bit, so a segment
/// plus its headers must stay under 65535. We cap well below that for headroom.
const MAX_SEGMENT_PAYLOAD: usize = 60_000;

/// Default per-segment payload size. A single large `SSL_write` is sliced into
/// segments of this size, mirroring real on-wire MSS behavior. 16 KiB keeps the
/// frame count low while staying comfortably under [`MAX_SEGMENT_PAYLOAD`].
pub const DEFAULT_SEGMENT_SIZE: usize = 16 * 1024;

/// Direction of a plaintext chunk, named from the connection's point of view.
/// `SSL_write` is the client emitting a request; `SSL_read` is the client
/// receiving a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDir {
    /// Client → server. Maps to an `SSL_write` (outbound request bytes).
    ClientToServer,
    /// Server → client. Maps to an `SSL_read` (inbound response bytes).
    ServerToClient,
}

/// The endpoints of a synthesized connection. `client` is whichever side issues
/// the HTTP request (the process running the SSL uprobe); `server` is the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnTuple {
    pub client: SocketAddr,
    pub server: SocketAddr,
}

/// Configuration for a [`FlowSynthesizer`].
#[derive(Debug, Clone)]
pub struct SynthConfig {
    /// `source_id` stamped on every emitted [`RawPacket`]. Routes the flow to a
    /// dispatcher shard and namespaces its `FlowKey`.
    pub source_id: String,
    /// Target payload size per synthesized TCP segment. Clamped to
    /// [`MAX_SEGMENT_PAYLOAD`]; zero falls back to [`DEFAULT_SEGMENT_SIZE`].
    pub segment_size: usize,
    /// Emit a synthetic SYN / SYN-ACK on [`FlowSynthesizer::open`] so the
    /// reassembler pins the client side deterministically. When false (or for a
    /// connection first seen mid-stream via [`FlowSynthesizer::data`]), the
    /// reassembler instead syncs on the first HTTP request line.
    pub emit_handshake: bool,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            source_id: "ebpf".to_string(),
            segment_size: DEFAULT_SEGMENT_SIZE,
            emit_handshake: true,
        }
    }
}

impl SynthConfig {
    fn effective_segment(&self) -> usize {
        match self.segment_size {
            0 => DEFAULT_SEGMENT_SIZE,
            n => n.min(MAX_SEGMENT_PAYLOAD),
        }
    }
}

/// Per-connection synthesis state: the endpoints and the next sequence number
/// to use in each direction.
#[derive(Debug)]
struct ConnState {
    tuple: ConnTuple,
    /// Next sequence number for client→server segments.
    seq_c2s: u32,
    /// Next sequence number for server→client segments.
    seq_s2c: u32,
}

impl ConnState {
    /// Initial sequence numbers. With a synthesized handshake the SYN carries
    /// ISN 0 and consumes one sequence number, so data starts at 1; without a
    /// handshake the absolute base is irrelevant (the reassembler re-baselines
    /// mid-stream), and 1 keeps the two paths identical.
    fn new(tuple: ConnTuple) -> Self {
        Self {
            tuple,
            seq_c2s: 1,
            seq_s2c: 1,
        }
    }

    fn seq_mut(&mut self, dir: StreamDir) -> &mut u32 {
        match dir {
            StreamDir::ClientToServer => &mut self.seq_c2s,
            StreamDir::ServerToClient => &mut self.seq_s2c,
        }
    }

    /// `(src, dst)` endpoints for a segment travelling in `dir`.
    fn endpoints(&self, dir: StreamDir) -> (SocketAddr, SocketAddr) {
        match dir {
            StreamDir::ClientToServer => (self.tuple.client, self.tuple.server),
            StreamDir::ServerToClient => (self.tuple.server, self.tuple.client),
        }
    }
}

/// Turns per-connection plaintext byte chunks into synthetic [`RawPacket`]s.
///
/// Lifecycle per connection:
/// 1. [`open`](Self::open) — register endpoints, optionally emit a handshake.
/// 2. [`data`](Self::data) — emit one or more TCP segments per chunk.
/// 3. [`close`](Self::close) — emit FINs so the reassembler finalizes promptly.
///
/// A connection first observed via [`data`](Self::data) (uprobe attached
/// mid-stream) is opened lazily with a [synthetic tuple](Self::synthetic_tuple)
/// and no handshake.
#[derive(Debug)]
pub struct FlowSynthesizer {
    cfg: SynthConfig,
    conns: HashMap<u64, ConnState>,
}

impl FlowSynthesizer {
    pub fn new(cfg: SynthConfig) -> Self {
        Self {
            cfg,
            conns: HashMap::new(),
        }
    }

    /// True if `conn_id` has been opened and not yet closed.
    pub fn is_open(&self, conn_id: u64) -> bool {
        self.conns.contains_key(&conn_id)
    }

    /// Number of live connections currently tracked.
    pub fn conn_count(&self) -> usize {
        self.conns.len()
    }

    /// Register a connection and, when [`SynthConfig::emit_handshake`] is set,
    /// emit a SYN / SYN-ACK pair. Re-opening an existing `conn_id` replaces its
    /// state (and re-handshakes) — the caller is expected to have closed the
    /// prior connection first.
    pub fn open(&mut self, conn_id: u64, tuple: ConnTuple, ts_us: i64) -> Vec<RawPacket> {
        let state = ConnState::new(tuple);
        let mut out = Vec::new();
        if self.cfg.emit_handshake {
            // SYN (client→server, ISN 0) then SYN-ACK (server→client, ISN 0).
            let (csrc, cdst) = state.endpoints(StreamDir::ClientToServer);
            out.push(self.frame(csrc, cdst, 0, 0, TCP_SYN, &[], ts_us));
            let (ssrc, sdst) = state.endpoints(StreamDir::ServerToClient);
            out.push(self.frame(ssrc, sdst, 0, 1, TCP_SYN | TCP_ACK, &[], ts_us));
        }
        self.conns.insert(conn_id, state);
        out
    }

    /// Emit TCP segments carrying `bytes` in direction `dir`. A chunk larger
    /// than the configured segment size is split across multiple in-order
    /// segments. An empty chunk emits nothing. Unknown connections are opened
    /// lazily (synthetic tuple, no handshake).
    pub fn data(
        &mut self,
        conn_id: u64,
        dir: StreamDir,
        bytes: &[u8],
        ts_us: i64,
    ) -> Vec<RawPacket> {
        if bytes.is_empty() {
            return Vec::new();
        }
        // Mid-stream attach (unknown conn): open lazily with a synthetic tuple
        // and no handshake. The reassembler re-baselines on the first request
        // line. Known connections keep their registered state untouched.
        self.conns
            .entry(conn_id)
            .or_insert_with(|| ConnState::new(Self::synthetic_tuple(conn_id)));

        let seg = self.cfg.effective_segment();
        // Snapshot the routing inputs while we hold the connection borrow, then
        // drop it so `self.frame` can borrow `self` immutably per segment.
        let (src, dst, mut seq) = {
            let state = self.conns.get(&conn_id).expect("just inserted");
            let (src, dst) = state.endpoints(dir);
            (src, dst, *self.peek_seq(conn_id, dir))
        };

        let mut out = Vec::with_capacity(bytes.len().div_ceil(seg));
        let mut offset = 0;
        while offset < bytes.len() {
            let end = (offset + seg).min(bytes.len());
            let chunk = &bytes[offset..end];
            out.push(self.frame(src, dst, seq, 1, TCP_PSH | TCP_ACK, chunk, ts_us));
            seq = seq.wrapping_add(chunk.len() as u32);
            offset = end;
        }

        *self.conns.get_mut(&conn_id).expect("present").seq_mut(dir) = seq;
        out
    }

    /// Emit FIN segments in both directions and forget the connection. The
    /// reassembler finalizes any pending response on FIN, so a turn does not
    /// have to wait out the 120 s idle sweep. A no-op for unknown connections.
    pub fn close(&mut self, conn_id: u64, ts_us: i64) -> Vec<RawPacket> {
        let Some(state) = self.conns.remove(&conn_id) else {
            return Vec::new();
        };
        let (csrc, cdst) = state.endpoints(StreamDir::ClientToServer);
        let (ssrc, sdst) = state.endpoints(StreamDir::ServerToClient);
        vec![
            self.frame(csrc, cdst, state.seq_c2s, 1, TCP_FIN | TCP_ACK, &[], ts_us),
            self.frame(ssrc, sdst, state.seq_s2c, 1, TCP_FIN | TCP_ACK, &[], ts_us),
        ]
    }

    /// Deterministic placeholder 5-tuple for a connection whose real socket
    /// addresses are unknown (uprobe gave `SSL*`/pid but no socket). Stable and
    /// collision-resistant per `conn_id`, so both directions of one connection
    /// share a `FlowKey`. The client side maps into `127.64.0.0/10` and the
    /// server into a documentation address; wire-API detection keys on the HTTP
    /// `Host`/path, not on these IPs, so they affect only the displayed tuple.
    pub fn synthetic_tuple(conn_id: u64) -> ConnTuple {
        let lo = conn_id as u32;
        let hi = (conn_id >> 32) as u32;
        // Client: 127.64.x.y to stay inside loopback's /8 but clear of 127.0.0.1.
        let client_ip = IpAddr::from([127, 64, (lo >> 8) as u8, lo as u8]);
        let client_port = 1024u16.wrapping_add((lo >> 16) as u16);
        // Server: 192.0.2.0/24 (TEST-NET-1, RFC 5737) — never a real host.
        let server_ip = IpAddr::from([192, 0, 2, (hi & 0xFF) as u8]);
        ConnTuple {
            client: SocketAddr::new(client_ip, client_port.max(1024)),
            server: SocketAddr::new(server_ip, 443),
        }
    }

    fn peek_seq(&self, conn_id: u64, dir: StreamDir) -> &u32 {
        let state = self.conns.get(&conn_id).expect("present");
        match dir {
            StreamDir::ClientToServer => &state.seq_c2s,
            StreamDir::ServerToClient => &state.seq_s2c,
        }
    }

    /// Build one Ethernet + IP + TCP frame around `payload`.
    #[allow(clippy::too_many_arguments)]
    fn frame(
        &self,
        src: SocketAddr,
        dst: SocketAddr,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
        ts_us: i64,
    ) -> RawPacket {
        debug_assert!(payload.len() <= MAX_SEGMENT_PAYLOAD);
        let ip_hdr_len = if src.is_ipv4() && dst.is_ipv4() {
            IPV4_HDR_LEN
        } else {
            IPV6_HDR_LEN
        };
        let mut data = Vec::with_capacity(ETH_HDR_LEN + ip_hdr_len + TCP_HDR_LEN + payload.len());

        // Ethernet II: zero MACs. The ethertype is a real IP type (never the
        // 0xFFFF heartbeat sentinel), so `is_heartbeat()` stays false.
        data.extend_from_slice(&[0u8; 12]);
        match (src.ip(), dst.ip()) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                data.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
                push_ipv4(&mut data, s.octets(), d.octets(), TCP_HDR_LEN + payload.len());
            }
            (s, d) => {
                let s = to_ipv6(s);
                let d = to_ipv6(d);
                data.extend_from_slice(&ETHERTYPE_IPV6.to_be_bytes());
                push_ipv6(&mut data, s, d, TCP_HDR_LEN + payload.len());
            }
        }
        push_tcp(&mut data, src.port(), dst.port(), seq, ack, flags);
        data.extend_from_slice(payload);

        let len = data.len() as u32;
        RawPacket {
            timestamp_us: ts_us,
            caplen: len,
            wirelen: len,
            link_type: LINKTYPE_ETHERNET,
            data: Bytes::from(data),
            source_id: self.cfg.source_id.clone(),
        }
    }
}

/// Map any IP into 16 IPv6 bytes (v4 → v4-mapped) so a mixed-family tuple still
/// produces a well-formed IPv6 frame. Matched families take the clean path.
fn to_ipv6(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V6(a) => a.octets(),
        IpAddr::V4(a) => a.to_ipv6_mapped().octets(),
    }
}

/// Push a 20-byte IPv4 header. `l4_len` is the TCP header + payload length, so
/// `total_length = 20 + l4_len` and the decoder recovers
/// `payload_length = total_length - 20 = l4_len`.
fn push_ipv4(buf: &mut Vec<u8>, src: [u8; 4], dst: [u8; 4], l4_len: usize) {
    let total_length = (IPV4_HDR_LEN + l4_len) as u16;
    let mut h = [0u8; IPV4_HDR_LEN];
    h[0] = 0x45; // version 4, IHL 5 (20 bytes)
    h[2..4].copy_from_slice(&total_length.to_be_bytes());
    h[8] = 64; // TTL
    h[9] = IP_PROTO_TCP;
    // checksum (h[10..12]) intentionally zero — not validated.
    h[12..16].copy_from_slice(&src);
    h[16..20].copy_from_slice(&dst);
    buf.extend_from_slice(&h);
}

/// Push a 40-byte IPv6 header. The `payload_length` field is the TCP header +
/// payload length, which the decoder uses directly.
fn push_ipv6(buf: &mut Vec<u8>, src: [u8; 16], dst: [u8; 16], l4_len: usize) {
    let payload_length = l4_len as u16;
    let mut h = [0u8; IPV6_HDR_LEN];
    h[0] = 0x60; // version 6
    h[4..6].copy_from_slice(&payload_length.to_be_bytes());
    h[6] = IP_PROTO_TCP; // next header
    h[7] = 64; // hop limit
    h[8..24].copy_from_slice(&src);
    h[24..40].copy_from_slice(&dst);
    buf.extend_from_slice(&h);
}

/// Push a 20-byte TCP header (no options, `data_offset = 5`).
fn push_tcp(buf: &mut Vec<u8>, src_port: u16, dst_port: u16, seq: u32, ack: u32, flags: u8) {
    let mut h = [0u8; TCP_HDR_LEN];
    h[0..2].copy_from_slice(&src_port.to_be_bytes());
    h[2..4].copy_from_slice(&dst_port.to_be_bytes());
    h[4..8].copy_from_slice(&seq.to_be_bytes());
    h[8..12].copy_from_slice(&ack.to_be_bytes());
    h[12] = 0x50; // data offset 5 → 20-byte header
    h[13] = flags;
    h[14..16].copy_from_slice(&0xFFFFu16.to_be_bytes()); // advertised window
                                                         // checksum + urgent ptr left zero.
    buf.extend_from_slice(&h);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4_tuple() -> ConnTuple {
        ConnTuple {
            client: "10.0.0.5:54321".parse().unwrap(),
            server: "93.184.216.34:443".parse().unwrap(),
        }
    }

    /// Parse the ethertype (bytes 12..14) from a synthesized Ethernet frame.
    fn ethertype(pkt: &RawPacket) -> u16 {
        u16::from_be_bytes([pkt.data[12], pkt.data[13]])
    }

    /// Extract `(seq, flags, payload)` from an IPv4 frame for assertions.
    fn ipv4_tcp(pkt: &RawPacket) -> (u32, u8, &[u8]) {
        let ip = ETH_HDR_LEN;
        let tcp = ip + IPV4_HDR_LEN;
        let seq = u32::from_be_bytes([
            pkt.data[tcp + 4],
            pkt.data[tcp + 5],
            pkt.data[tcp + 6],
            pkt.data[tcp + 7],
        ]);
        let flags = pkt.data[tcp + 13];
        let payload = &pkt.data[tcp + TCP_HDR_LEN..];
        (seq, flags, payload)
    }

    #[test]
    fn open_with_handshake_emits_syn_synack() {
        let mut s = FlowSynthesizer::new(SynthConfig::default());
        let frames = s.open(1, v4_tuple(), 1_000);
        assert_eq!(frames.len(), 2);
        let (_, syn_flags, syn_pl) = ipv4_tcp(&frames[0]);
        assert_eq!(syn_flags, TCP_SYN);
        assert!(syn_pl.is_empty());
        let (_, sa_flags, sa_pl) = ipv4_tcp(&frames[1]);
        assert_eq!(sa_flags, TCP_SYN | TCP_ACK);
        assert!(sa_pl.is_empty());
        assert!(!frames[0].is_heartbeat() && !frames[1].is_heartbeat());
    }

    #[test]
    fn open_without_handshake_emits_nothing() {
        let cfg = SynthConfig {
            emit_handshake: false,
            ..Default::default()
        };
        let mut s = FlowSynthesizer::new(cfg);
        assert!(s.open(1, v4_tuple(), 0).is_empty());
        assert!(s.is_open(1));
    }

    #[test]
    fn data_emits_single_segment_with_payload_and_advancing_seq() {
        let mut s = FlowSynthesizer::new(SynthConfig::default());
        s.open(1, v4_tuple(), 0);
        let body = b"GET / HTTP/1.1\r\n\r\n";
        let frames = s.data(1, StreamDir::ClientToServer, body, 10);
        assert_eq!(frames.len(), 1);
        let (seq, flags, payload) = ipv4_tcp(&frames[0]);
        assert_eq!(seq, 1, "first data seq follows SYN's ISN+1");
        assert_eq!(flags, TCP_PSH | TCP_ACK);
        assert_eq!(payload, body);
        assert_eq!(ethertype(&frames[0]), ETHERTYPE_IPV4);

        // Next chunk continues from the advanced seq.
        let more = s.data(1, StreamDir::ClientToServer, b"XYZ", 20);
        let (seq2, _, _) = ipv4_tcp(&more[0]);
        assert_eq!(seq2, 1 + body.len() as u32);
    }

    #[test]
    fn large_chunk_splits_into_segments_with_contiguous_seq() {
        let cfg = SynthConfig {
            segment_size: 1000,
            ..Default::default()
        };
        let mut s = FlowSynthesizer::new(cfg);
        s.open(1, v4_tuple(), 0);
        let body = vec![b'A'; 2500];
        let frames = s.data(1, StreamDir::ServerToClient, &body, 5);
        assert_eq!(frames.len(), 3, "2500 / 1000 = 3 segments");

        let mut expected_seq = 1u32;
        let mut total = 0usize;
        for f in &frames {
            let (seq, _, payload) = ipv4_tcp(f);
            assert_eq!(seq, expected_seq);
            assert!(payload.len() <= 1000);
            expected_seq += payload.len() as u32;
            total += payload.len();
        }
        assert_eq!(total, 2500, "no payload bytes lost across segments");
    }

    #[test]
    fn empty_chunk_emits_nothing() {
        let mut s = FlowSynthesizer::new(SynthConfig::default());
        s.open(1, v4_tuple(), 0);
        assert!(s.data(1, StreamDir::ClientToServer, &[], 0).is_empty());
    }

    #[test]
    fn unknown_conn_opens_lazily_without_handshake() {
        let mut s = FlowSynthesizer::new(SynthConfig::default());
        // No open() first — data() must lazily register the connection.
        let frames = s.data(7, StreamDir::ClientToServer, b"POST / HTTP/1.1\r\n", 0);
        assert_eq!(frames.len(), 1, "exactly the data segment, no handshake");
        assert!(s.is_open(7));
    }

    #[test]
    fn close_emits_fin_both_directions_and_forgets_conn() {
        let mut s = FlowSynthesizer::new(SynthConfig::default());
        s.open(1, v4_tuple(), 0);
        let frames = s.close(1, 99);
        assert_eq!(frames.len(), 2);
        for f in &frames {
            let (_, flags, pl) = ipv4_tcp(f);
            assert_eq!(flags, TCP_FIN | TCP_ACK);
            assert!(pl.is_empty());
        }
        assert!(!s.is_open(1));
        assert!(s.close(1, 100).is_empty(), "second close is a no-op");
    }

    #[test]
    fn ipv6_tuple_produces_ipv6_ethertype_and_header() {
        let cfg = SynthConfig::default();
        let mut s = FlowSynthesizer::new(cfg);
        let tuple = ConnTuple {
            client: "[2001:db8::1]:50000".parse().unwrap(),
            server: "[2606:4700::1]:443".parse().unwrap(),
        };
        s.open(1, tuple, 0);
        let frames = s.data(1, StreamDir::ClientToServer, b"GET / HTTP/1.1\r\n", 0);
        assert_eq!(ethertype(&frames[0]), ETHERTYPE_IPV6);
        // Ethernet(14) + IPv6(40) + TCP(20) + payload.
        assert_eq!(frames[0].data.len(), ETH_HDR_LEN + IPV6_HDR_LEN + TCP_HDR_LEN + 16);
    }

    #[test]
    fn synthetic_tuple_is_stable_and_distinct() {
        let a1 = FlowSynthesizer::synthetic_tuple(42);
        let a2 = FlowSynthesizer::synthetic_tuple(42);
        let b = FlowSynthesizer::synthetic_tuple(43);
        assert_eq!(a1, a2, "same conn_id → same tuple");
        assert_ne!(a1, b, "different conn_id → different tuple");
    }

    #[test]
    fn segment_size_zero_falls_back_to_default() {
        let cfg = SynthConfig {
            segment_size: 0,
            ..Default::default()
        };
        let mut s = FlowSynthesizer::new(cfg);
        s.open(1, v4_tuple(), 0);
        let body = vec![b'Z'; DEFAULT_SEGMENT_SIZE + 1];
        let frames = s.data(1, StreamDir::ClientToServer, &body, 0);
        assert_eq!(frames.len(), 2, "default segment size still chunks");
    }
}
