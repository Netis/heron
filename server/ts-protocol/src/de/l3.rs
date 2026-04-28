use std::net::IpAddr;

use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;
use super::{try_consume, try_skip};

/// L3 (IP) information extracted from the packet header.
#[derive(Debug)]
pub struct L3Info {
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub protocol: u8,
    /// Length (bytes) of the L3 payload as declared by the IP header — the
    /// transport segment length on the wire, regardless of capture truncation.
    /// IPv4: `total_length - ihl`. IPv6: header `payload_length`.
    pub payload_length: u32,
}

/// Dispatch L3 parsing based on `ether_type` and advance the buffer past the
/// IP header. Returns `L3Info` with the source/destination IPs and the
/// transport-layer protocol number.
pub fn dispatch_l3(buf: &mut PacketBuf, ether_type: u16) -> DecodeResult<L3Info> {
    match ether_type {
        ETHERTYPE_IPV4 => decode_ipv4(buf),
        ETHERTYPE_IPV6 => decode_ipv6(buf),
        _ => Err(DecodeError::NotIp),
    }
}

fn decode_ipv4(buf: &mut PacketBuf) -> DecodeResult<L3Info> {
    let ip = try_consume!(buf, Ipv4Header);
    let ihl = ip.ihl();
    if ihl < 20 {
        return Err(DecodeError::InvalidHeader);
    }

    let src_ip = IpAddr::V4(ip.src_ip());
    let dst_ip = IpAddr::V4(ip.dst_ip());
    let protocol = ip.protocol();
    let total_len = ip.total_length() as usize;

    // Skip IPv4 options if IHL > 20
    if ihl > 20 {
        try_skip!(buf, ihl - 20);
    }

    // Truncate buffer to IP total_length
    let ip_start = buf.offset() - ihl;
    if total_len >= ihl {
        buf.set_len(ip_start + total_len);
    }

    Ok(L3Info {
        src_ip,
        dst_ip,
        protocol,
        payload_length: (total_len as u32).saturating_sub(ihl as u32),
    })
}

fn decode_ipv6(buf: &mut PacketBuf) -> DecodeResult<L3Info> {
    let ip = try_consume!(buf, Ipv6Header);
    let payload_length = ip.payload_length();

    let src_ip = IpAddr::V6(ip.src_ip());
    let dst_ip = IpAddr::V6(ip.dst_ip());
    let protocol = ip.next_header();

    // Truncate buffer to IPv6 header + payload_length
    let ip_start = buf.offset() - 40;
    buf.set_len(ip_start + 40 + payload_length as usize);

    Ok(L3Info {
        src_ip,
        dst_ip,
        protocol,
        payload_length: payload_length as u32,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn make_ipv4(protocol: u8, src: [u8; 4], dst: [u8; 4], total_length: u16) -> Vec<u8> {
        let mut hdr = vec![0u8; 20];
        hdr[0] = 0x45; // version=4, IHL=5 (20 bytes)
        hdr[1] = 0; // DSCP/ECN
        let tl = total_length.to_be_bytes();
        hdr[2] = tl[0];
        hdr[3] = tl[1];
        // identification, flags, frag offset, ttl = 0
        hdr[9] = protocol;
        // checksum = 0
        hdr[12..16].copy_from_slice(&src);
        hdr[16..20].copy_from_slice(&dst);
        hdr
    }

    fn make_ipv6(next_header: u8, src: [u8; 16], dst: [u8; 16], payload_length: u16) -> Vec<u8> {
        let mut hdr = vec![0u8; 40];
        hdr[0] = 0x60; // version=6, TC/FL=0
        let pl = payload_length.to_be_bytes();
        hdr[4] = pl[0];
        hdr[5] = pl[1];
        hdr[6] = next_header;
        hdr[7] = 0; // hop limit
        hdr[8..24].copy_from_slice(&src);
        hdr[24..40].copy_from_slice(&dst);
        hdr
    }

    #[test]
    fn ipv4_standard() {
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 40);
        data.extend_from_slice(&[0u8; 20]);
        let mut buf = PacketBuf::new(&data);
        let info = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap();
        assert_eq!(info.src_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(info.dst_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(info.protocol, 6);
        assert_eq!(buf.offset(), 20);
    }

    #[test]
    fn ipv4_with_options() {
        // IHL=6 means 24-byte header (5*4=20 base + 4 option bytes)
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 44);
        data[0] = 0x46; // IHL=6
                        // Insert 4 option bytes after the 20-byte base header
        data.splice(20..20, [0u8; 4]);
        data.extend_from_slice(&[0u8; 20]); // payload
        let mut buf = PacketBuf::new(&data);
        let info = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap();
        assert_eq!(info.protocol, 6);
        assert_eq!(buf.offset(), 24);
    }

    #[test]
    fn ipv4_invalid_ihl() {
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 40);
        data[0] = 0x43; // IHL=3 (12 bytes, invalid < 20)
        let mut buf = PacketBuf::new(&data);
        let err = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap_err();
        assert_eq!(err, DecodeError::InvalidHeader);
    }

    #[test]
    fn ipv4_total_length_truncates_padding() {
        // total_length=30 means only 10 bytes of payload after 20-byte header
        // but we provide 40 bytes of extra data (padding)
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 30);
        data.extend_from_slice(&[0u8; 40]);
        let mut buf = PacketBuf::new(&data);
        let _info = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap();
        assert_eq!(buf.remaining(), 10);
    }

    #[test]
    fn ipv4_truncated() {
        let data = [0u8; 2];
        let mut buf = PacketBuf::new(&data);
        let err = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap_err();
        assert_eq!(err, DecodeError::Truncated);
    }

    #[test]
    fn ipv6_standard() {
        let src = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let dst = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let mut data = make_ipv6(6, src, dst, 20);
        data.extend_from_slice(&[0u8; 20]);
        let mut buf = PacketBuf::new(&data);
        let info = dispatch_l3(&mut buf, ETHERTYPE_IPV6).unwrap();
        assert_eq!(info.src_ip, IpAddr::V6(Ipv6Addr::from(src)));
        assert_eq!(info.dst_ip, IpAddr::V6(Ipv6Addr::from(dst)));
        assert_eq!(info.protocol, 6);
        assert_eq!(buf.offset(), 40);
    }

    #[test]
    fn ipv6_truncated() {
        let data = [0u8; 20];
        let mut buf = PacketBuf::new(&data);
        let err = dispatch_l3(&mut buf, ETHERTYPE_IPV6).unwrap_err();
        assert_eq!(err, DecodeError::Truncated);
    }

    #[test]
    fn non_ip_ether_type() {
        let data = [0u8; 20];
        let mut buf = PacketBuf::new(&data);
        let err = dispatch_l3(&mut buf, 0x0806).unwrap_err();
        assert_eq!(err, DecodeError::NotIp);
    }
}
