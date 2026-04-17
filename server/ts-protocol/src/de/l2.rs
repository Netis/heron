use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;
use super::try_consume;

/// Decode the L2 header for the given `link_type` and advance the buffer past
/// it. Returns the EtherType (or equivalent protocol number) that identifies
/// the L3 payload.
pub fn decode_l2(buf: &mut PacketBuf, link_type: u32) -> DecodeResult<u16> {
    match link_type {
        LINKTYPE_ETHERNET => decode_ethernet(buf),
        LINKTYPE_RAW => detect_raw_ip(buf),
        LINKTYPE_NULL => decode_null(buf),
        LINKTYPE_LINUX_SLL => decode_linux_sll(buf),
        LINKTYPE_LINUX_SLL2 => decode_linux_sll2(buf),
        _ => Err(DecodeError::NotSupported),
    }
}

fn decode_ethernet(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let hdr = try_consume!(buf, EthernetHeader);
    let ether_type = hdr.ether_type();
    resolve_next(buf, ether_type)
}

/// Recursively strip VLAN/QinQ tags and MPLS label stacks, returning the
/// final EtherType (or IP pseudo-EtherType) that identifies the L3 payload.
fn resolve_next(buf: &mut PacketBuf, ether_type: u16) -> DecodeResult<u16> {
    match ether_type {
        ETHERTYPE_VLAN | ETHERTYPE_QINQ => {
            let vlan = try_consume!(buf, VlanHeader);
            let next_type = vlan.ether_type();
            resolve_next(buf, next_type)
        }
        ETHERTYPE_MPLS => strip_mpls(buf),
        other => Ok(other),
    }
}

/// Consume MPLS label shims until the bottom-of-stack is reached, then peek
/// the first nibble of the next byte to distinguish IPv4 from IPv6.
fn strip_mpls(buf: &mut PacketBuf) -> DecodeResult<u16> {
    loop {
        let label = try_consume!(buf, MplsHeader);
        if label.bottom_of_stack() {
            break;
        }
    }
    let byte = buf.peek::<u8>().ok_or(DecodeError::Truncated)?;
    match byte >> 4 {
        4 => Ok(ETHERTYPE_IPV4),
        6 => Ok(ETHERTYPE_IPV6),
        _ => Err(DecodeError::NotIp),
    }
}

fn decode_null(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let hdr = try_consume!(buf, NullHeader);
    match hdr.af_family() {
        AF_INET => Ok(ETHERTYPE_IPV4),
        AF_INET6_BSD | AF_INET6_LINUX => Ok(ETHERTYPE_IPV6),
        _ => Err(DecodeError::NotIp),
    }
}

fn detect_raw_ip(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let byte = buf.peek::<u8>().ok_or(DecodeError::Truncated)?;
    match byte >> 4 {
        4 => Ok(ETHERTYPE_IPV4),
        6 => Ok(ETHERTYPE_IPV6),
        _ => Err(DecodeError::NotIp),
    }
}

fn decode_linux_sll(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let hdr = try_consume!(buf, LinuxSllHeader);
    let proto = hdr.protocol();
    if proto == ETHERTYPE_IPV4 || proto == ETHERTYPE_IPV6 {
        Ok(proto)
    } else {
        Err(DecodeError::NotIp)
    }
}

fn decode_linux_sll2(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let hdr = try_consume!(buf, LinuxSll2Header);
    let proto = hdr.protocol();
    if proto == ETHERTYPE_IPV4 || proto == ETHERTYPE_IPV6 {
        Ok(proto)
    } else {
        Err(DecodeError::NotIp)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::de::buf::PacketBuf;

    // --- Ethernet ---

    #[test]
    fn ethernet_ipv4() {
        let mut data = [0u8; 14];
        data[12] = 0x08;
        data[13] = 0x00;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 14);
    }

    #[test]
    fn ethernet_ipv6() {
        let mut data = [0u8; 14];
        data[12] = 0x86;
        data[13] = 0xDD;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn ethernet_vlan_ipv4() {
        // 12 zero bytes (dst+src MAC) + 0x8100 (VLAN) + TCI + 0x0800 (IPv4)
        let mut data = [0u8; 18];
        data[12] = 0x81;
        data[13] = 0x00;
        // TCI at 14-15 (already zero)
        data[16] = 0x08;
        data[17] = 0x00;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 18);
    }

    #[test]
    fn ethernet_qinq_ipv4() {
        // 12 zeros + 0x88A8 (QinQ) + TCI + 0x8100 (VLAN) + TCI + 0x0800 (IPv4)
        let mut data = [0u8; 22];
        data[12] = 0x88;
        data[13] = 0xA8;
        // outer TCI at 14-15 (zero)
        data[16] = 0x81;
        data[17] = 0x00;
        // inner TCI at 18-19 (zero)
        data[20] = 0x08;
        data[21] = 0x00;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 22);
    }

    #[test]
    fn ethernet_arp_returns_ok_0806() {
        let mut data = [0u8; 14];
        data[12] = 0x08;
        data[13] = 0x06;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(0x0806));
    }

    #[test]
    fn ethernet_truncated() {
        let data = [0u8; 10];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_ETHERNET),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn vlan_truncated() {
        // 12 zeros + VLAN tag start, but only 1 byte of VLAN header
        let mut data = [0u8; 14];
        data[12] = 0x81;
        data[13] = 0x00;
        // only 1 extra byte, VLAN header needs 4
        let data = &data[..13]; // 12 MAC + 1 byte of ether_type (truncated)
        let mut buf = PacketBuf::new(data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_ETHERNET),
            Err(DecodeError::Truncated)
        );
    }

    // --- Raw IP ---

    #[test]
    fn raw_ip_v4() {
        let data = [0x45u8]; // version=4, IHL=5
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_RAW), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 0); // peek doesn't advance
    }

    #[test]
    fn raw_ip_v6() {
        let data = [0x60u8]; // version=6
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_RAW), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn raw_ip_empty() {
        let data: [u8; 0] = [];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_RAW),
            Err(DecodeError::Truncated)
        );
    }

    // --- NULL / BSD loopback ---

    #[test]
    fn null_ipv4() {
        let data = AF_INET.to_ne_bytes();
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_NULL), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 4);
    }

    #[test]
    fn null_ipv6_bsd() {
        let data = AF_INET6_BSD.to_ne_bytes();
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_NULL), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn null_non_ip() {
        let data = 999u32.to_ne_bytes();
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_NULL), Err(DecodeError::NotIp));
    }

    // --- Linux SLL (v1) ---

    #[test]
    fn sll_ipv4() {
        let mut data = [0u8; 16];
        data[14] = 0x08;
        data[15] = 0x00;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_LINUX_SLL), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 16);
    }

    #[test]
    fn sll_non_ip() {
        let mut data = [0u8; 16];
        data[14] = 0x08;
        data[15] = 0x06; // ARP
        let mut buf = PacketBuf::new(&data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_LINUX_SLL),
            Err(DecodeError::NotIp)
        );
    }

    #[test]
    fn sll_truncated() {
        let data = [0u8; 10];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_LINUX_SLL),
            Err(DecodeError::Truncated)
        );
    }

    // --- Linux SLL2 (v2) ---

    #[test]
    fn sll2_ipv4() {
        let mut data = [0u8; 20];
        data[0] = 0x08;
        data[1] = 0x00;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_LINUX_SLL2), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 20);
    }

    #[test]
    fn sll2_non_ip() {
        let mut data = [0u8; 20];
        data[0] = 0x08;
        data[1] = 0x06; // ARP
        let mut buf = PacketBuf::new(&data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_LINUX_SLL2),
            Err(DecodeError::NotIp)
        );
    }

    // --- Unsupported link type ---

    #[test]
    fn unsupported_link_type() {
        let data = [0u8; 20];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, 999), Err(DecodeError::NotSupported));
    }

    // --- MPLS ---

    /// Build an MPLS label shim. `bottom=true` sets the S bit.
    fn mpls_label(bottom: bool) -> [u8; 4] {
        let mut b = [0u8; 4];
        if bottom {
            b[2] |= 0x01;
        }
        b
    }

    #[test]
    fn ethernet_mpls_ipv4() {
        // 12 zero MACs + 0x8847 (MPLS) + 4-byte label (S=1) + IPv4 version nibble
        let mut data = [0u8; 19];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(true));
        data[18] = 0x45; // IPv4, IHL=5
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 18);
    }

    #[test]
    fn ethernet_mpls_ipv6() {
        let mut data = [0u8; 19];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(true));
        data[18] = 0x60; // IPv6
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn ethernet_vlan_mpls_ipv4() {
        // Eth + VLAN(tpid=0x8100, inner=0x8847) + MPLS(S=1) + IPv4
        let mut data = [0u8; 23];
        data[12] = 0x81;
        data[13] = 0x00;
        // TCI at 14-15 (zero)
        data[16] = 0x88;
        data[17] = 0x47;
        data[18..22].copy_from_slice(&mpls_label(true));
        data[22] = 0x45;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 22);
    }

    #[test]
    fn ethernet_mpls_multilabel_ipv4() {
        // Two MPLS labels (first with S=0, second with S=1) then IPv4
        let mut data = [0u8; 23];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(false));
        data[18..22].copy_from_slice(&mpls_label(true));
        data[22] = 0x45;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 22);
    }

    #[test]
    fn ethernet_mpls_non_ip_returns_not_ip() {
        // Bottom label followed by a byte whose top nibble is neither 4 nor 6
        let mut data = [0u8; 19];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(true));
        data[18] = 0x00;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_ETHERNET),
            Err(DecodeError::NotIp)
        );
    }

    #[test]
    fn ethernet_mpls_truncated_shim() {
        // Eth + MPLS ether_type but only 2 bytes of shim present
        let mut data = [0u8; 16];
        data[12] = 0x88;
        data[13] = 0x47;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(
            decode_l2(&mut buf, LINKTYPE_ETHERNET),
            Err(DecodeError::Truncated)
        );
    }
}
