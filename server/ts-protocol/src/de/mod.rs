pub mod buf;
pub mod error;
pub mod headers;
pub mod l2;
pub mod l3;
pub mod l4;

use bytes::Bytes;

use crate::net::{Direction, FlowKey, ParsedPacket};

use self::{buf::PacketBuf, l2::decode_l2, l3::dispatch_l3, l4::dispatch_l4};

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

/// Consume a typed header from a `PacketBuf`, returning
/// `Err(DecodeError::Truncated)` if the buffer has insufficient bytes.
macro_rules! try_consume {
    ($buf:expr, $H:ty) => {
        $buf.consume::<$H>()
            .ok_or(crate::de::error::DecodeError::Truncated)?
    };
}

/// Advance the cursor by `$n` bytes, returning `Err(DecodeError::Truncated)`
/// if fewer bytes remain.
macro_rules! try_skip {
    ($buf:expr, $n:expr) => {{
        let n: usize = $n;
        if $buf.remaining() < n {
            return Err(crate::de::error::DecodeError::Truncated);
        }
        $buf.advance(n);
    }};
}

pub(crate) use try_consume;
pub(crate) use try_skip;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Attempt to decode a raw packet into a [`ParsedPacket`].
///
/// Returns `Err` with the specific [`DecodeError`] variant when the packet
/// cannot be decoded. Callers can match on the variant to attribute drops
/// (unsupported link type, non-IP, non-TCP, truncated, malformed).
pub fn decode(
    data: &[u8],
    link_type: u32,
    timestamp_us: i64,
    source_id: String,
) -> crate::de::error::DecodeResult<ParsedPacket> {
    let mut buf = PacketBuf::new(data);

    let ether_type = decode_l2(&mut buf, link_type)?;
    let l3 = dispatch_l3(&mut buf, ether_type)?;
    let l4 = dispatch_l4(&mut buf, l3.protocol)?;

    let payload = Bytes::copy_from_slice(buf.remaining_slice());
    // Wire segment length comes from the (intact, header-front) IP+TCP header
    // fields. When snaplen truncation drops bytes off the tail, `payload.len()`
    // is short of this — the reassembler uses `wire_payload_len` for seq math
    // so the per-direction byte stream stays synchronized regardless.
    let wire_payload_len = l3.payload_length.saturating_sub(l4.header_length);

    let flow_key = FlowKey::new(source_id, l3.src_ip, l4.src_port, l3.dst_ip, l4.dst_port);
    let direction = if (l3.src_ip, l4.src_port) <= (l3.dst_ip, l4.dst_port) {
        Direction::AtoB
    } else {
        Direction::BtoA
    };

    Ok(ParsedPacket {
        flow_key,
        direction,
        src_ip: l3.src_ip,
        src_port: l4.src_port,
        dst_ip: l3.dst_ip,
        dst_port: l4.dst_port,
        tcp_flags: l4.flags,
        tcp_seq: l4.seq,
        tcp_ack: l4.ack,
        payload,
        wire_payload_len,
        timestamp_us,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Direction;
    use headers::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn ethernet_hdr(ether_type: u16) -> Vec<u8> {
        let mut h = vec![0u8; 12]; // dst + src MAC
        h.extend_from_slice(&ether_type.to_be_bytes());
        h
    }

    fn vlan_tag(ether_type: u16) -> Vec<u8> {
        let mut h = vec![0x00, 0x01]; // TCI
        h.extend_from_slice(&ether_type.to_be_bytes());
        h
    }

    fn ipv4_hdr(protocol: u8, src: [u8; 4], dst: [u8; 4], payload_len: usize) -> Vec<u8> {
        let total_length = (20 + payload_len) as u16;
        let mut h = vec![0u8; 20];
        h[0] = 0x45;
        let tl = total_length.to_be_bytes();
        h[2] = tl[0];
        h[3] = tl[1];
        h[9] = protocol;
        h[12..16].copy_from_slice(&src);
        h[16..20].copy_from_slice(&dst);
        h
    }

    fn ipv6_hdr(next_header: u8, src: [u8; 16], dst: [u8; 16], payload_length: u16) -> Vec<u8> {
        let mut h = vec![0u8; 40];
        h[0] = 0x60;
        let pl = payload_length.to_be_bytes();
        h[4] = pl[0];
        h[5] = pl[1];
        h[6] = next_header;
        h[8..24].copy_from_slice(&src);
        h[24..40].copy_from_slice(&dst);
        h
    }

    fn tcp_hdr(src_port: u16, dst_port: u16, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
        let mut h = vec![0u8; 20];
        let sp = src_port.to_be_bytes();
        h[0] = sp[0];
        h[1] = sp[1];
        let dp = dst_port.to_be_bytes();
        h[2] = dp[0];
        h[3] = dp[1];
        let s = seq.to_be_bytes();
        h[4..8].copy_from_slice(&s);
        let a = ack.to_be_bytes();
        h[8..12].copy_from_slice(&a);
        h[12] = 0x50; // data_offset = 5
        h[13] = flags;
        h
    }

    #[test]
    fn decode_eth_ipv4_tcp() {
        let payload = b"GET / HTTP/1.1\r\n";
        let src = [10, 0, 0, 1];
        let dst = [10, 0, 0, 2];
        let tcp = tcp_hdr(12345, 80, 1000, 0, 0x18);
        let ip = ipv4_hdr(6, src, dst, tcp.len() + payload.len());
        let eth = ethernet_hdr(ETHERTYPE_IPV4);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);
        pkt.extend_from_slice(payload);

        let result =
            decode(&pkt, LINKTYPE_ETHERNET, 1234567890, String::new()).expect("should decode");

        assert_eq!(result.src_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(result.dst_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(result.src_port, 12345);
        assert_eq!(result.dst_port, 80);
        assert_eq!(result.tcp_seq, 1000);
        assert_eq!(result.tcp_ack, 0);
        assert_eq!(result.tcp_flags, 0x18);
        assert_eq!(result.payload.as_ref(), payload);
        assert_eq!(result.timestamp_us, 1234567890);
        assert_eq!(result.direction, Direction::AtoB);
    }

    #[test]
    fn decode_eth_ipv6_tcp() {
        let payload = b"hello";
        let mut src = [0u8; 16];
        src[0] = 0x20;
        src[1] = 0x01;
        src[2] = 0x0d;
        src[3] = 0xb8;
        src[15] = 0x01;
        let mut dst = [0u8; 16];
        dst[0] = 0x20;
        dst[1] = 0x01;
        dst[2] = 0x0d;
        dst[3] = 0xb8;
        dst[15] = 0x02;

        let tcp = tcp_hdr(443, 50000, 0, 0, 0x02);
        let tcp_payload_len = (tcp.len() + payload.len()) as u16;
        let ip = ipv6_hdr(6, src, dst, tcp_payload_len);
        let eth = ethernet_hdr(ETHERTYPE_IPV6);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);
        pkt.extend_from_slice(payload);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 0, String::new()).expect("should decode");

        let expected_src = IpAddr::V6(Ipv6Addr::from(src));
        let expected_dst = IpAddr::V6(Ipv6Addr::from(dst));
        assert_eq!(result.src_ip, expected_src);
        assert_eq!(result.dst_ip, expected_dst);
        assert_eq!(result.src_port, 443);
        assert_eq!(result.dst_port, 50000);
        assert_eq!(result.payload.as_ref(), payload);
    }

    #[test]
    fn decode_vlan_ipv4_tcp() {
        let src = [192, 168, 1, 1];
        let dst = [192, 168, 1, 2];
        let tcp = tcp_hdr(80, 443, 0, 0, 0x02);
        let ip = ipv4_hdr(6, src, dst, tcp.len());
        let eth = ethernet_hdr(ETHERTYPE_VLAN);
        let vlan = vlan_tag(ETHERTYPE_IPV4);

        let mut pkt = eth;
        pkt.extend_from_slice(&vlan);
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 0, String::new()).expect("should decode");

        assert_eq!(result.src_ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(result.src_port, 80);
        assert_eq!(result.dst_port, 443);
    }

    #[test]
    fn decode_raw_ipv4_tcp() {
        let src = [10, 1, 0, 1];
        let dst = [10, 1, 0, 2];
        let tcp = tcp_hdr(8080, 80, 0, 0, 0x10);
        let ip = ipv4_hdr(6, src, dst, tcp.len());

        let mut pkt = ip;
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_RAW, 0, String::new()).expect("should decode");

        assert_eq!(result.src_ip, IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1)));
        assert_eq!(result.src_port, 8080);
        assert_eq!(result.dst_port, 80);
    }

    #[test]
    fn decode_sll_ipv4_tcp() {
        let src = [172, 16, 0, 1];
        let dst = [172, 16, 0, 2];
        let tcp = tcp_hdr(9090, 80, 0, 0, 0x10);
        let ip = ipv4_hdr(6, src, dst, tcp.len());

        // SLL header: 16 bytes total; protocol field at bytes 14-15
        let mut sll = vec![0u8; 14];
        sll.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes()); // protocol field

        let mut pkt = sll;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_LINUX_SLL, 0, String::new()).expect("should decode");

        assert_eq!(result.src_port, 9090);
    }

    #[test]
    fn decode_direction_btoa() {
        // src=(10.0.0.2,80), dst=(10.0.0.1,443)
        // Since (10.0.0.1,443) < (10.0.0.2,80), addr_a is (10.0.0.1,443)
        // and the packet goes from addr_b to addr_a → BtoA
        let src = [10, 0, 0, 2];
        let dst = [10, 0, 0, 1];
        let tcp = tcp_hdr(80, 443, 0, 0, 0x10);
        let ip = ipv4_hdr(6, src, dst, tcp.len());
        let eth = ethernet_hdr(ETHERTYPE_IPV4);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 0, String::new()).expect("should decode");

        assert_eq!(result.direction, Direction::BtoA);
        // addr_a should be the smaller of the two endpoints
        let addr_a_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(result.flow_key.addr_a.0, addr_a_ip);
    }

    #[test]
    fn decode_non_tcp_returns_err_not_tcp() {
        let src = [10, 0, 0, 1];
        let dst = [10, 0, 0, 2];
        // protocol=17 (UDP)
        let ip = ipv4_hdr(17, src, dst, 8);
        let eth = ethernet_hdr(ETHERTYPE_IPV4);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&[0u8; 8]); // dummy UDP payload

        assert_eq!(
            decode(&pkt, LINKTYPE_ETHERNET, 0, String::new()).unwrap_err(),
            crate::de::error::DecodeError::NotTcp
        );
    }

    #[test]
    fn decode_truncated_returns_err_truncated() {
        let pkt = [0u8; 5];
        assert_eq!(
            decode(&pkt, LINKTYPE_ETHERNET, 0, String::new()).unwrap_err(),
            crate::de::error::DecodeError::Truncated
        );
    }

    #[test]
    fn decode_unsupported_link_type_returns_err_not_supported() {
        let pkt = [0u8; 64];
        assert_eq!(
            decode(&pkt, 999, 0, String::new()).unwrap_err(),
            crate::de::error::DecodeError::NotSupported
        );
    }

    #[test]
    fn wire_payload_len_reflects_ip_header_not_captured_bytes() {
        // Build a frame whose IP total_length declares a 100-byte TCP segment
        // payload, but only feed the decoder the first 50 bytes of payload —
        // simulating a snaplen-truncated capture. The decoded packet's
        // `payload.len()` is 50, while `wire_payload_len` must report the
        // 100-byte on-wire length.
        let src = [10, 0, 0, 1];
        let dst = [10, 0, 0, 2];
        let tcp = tcp_hdr(12345, 80, 0, 0, 0x18);
        // total_length = ip_hdr(20) + tcp_hdr(20) + 100 wire-payload = 140.
        let ip = ipv4_hdr(6, src, dst, tcp.len() + 100);
        let eth = ethernet_hdr(ETHERTYPE_IPV4);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);
        // Only 50 bytes of body present in the captured slice.
        pkt.extend_from_slice(&[0u8; 50]);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 0, String::new()).expect("should decode");
        assert_eq!(result.payload.len(), 50, "captured payload only");
        assert_eq!(
            result.wire_payload_len, 100,
            "wire payload length comes from IP+TCP header, not capture"
        );
    }

    #[test]
    fn wire_payload_len_matches_payload_len_for_intact_packet() {
        let payload = b"hello world";
        let src = [10, 0, 0, 1];
        let dst = [10, 0, 0, 2];
        let tcp = tcp_hdr(12345, 80, 0, 0, 0x18);
        let ip = ipv4_hdr(6, src, dst, tcp.len() + payload.len());
        let eth = ethernet_hdr(ETHERTYPE_IPV4);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);
        pkt.extend_from_slice(payload);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 0, String::new()).expect("should decode");
        assert_eq!(result.payload.len() as u32, result.wire_payload_len);
    }
}
