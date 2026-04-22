use bytes::Bytes;

/// Sentinel EtherType used by cloud-probe (and mimicked by pcap-live) to mark
/// a [`RawPacket`] as a heartbeat signal rather than real traffic. Matches
/// cloud-probe's `ZMQ_HEARTBEAT_ETHER_TYPE` compile-time constant.
pub const HEARTBEAT_ETHER_TYPE: u16 = 0xFFFF;

/// Minimum length (Ethernet II header) of a heartbeat packet on the wire.
pub const HEARTBEAT_PACKET_LEN: usize = 14;

/// A raw captured packet before any protocol parsing.
///
/// Heartbeats are encoded as a sentinel `RawPacket`: 14-byte Ethernet header
/// with all-zero source/dest MACs and `ether_type = 0xFFFF`. Detection is
/// via [`RawPacket::is_heartbeat`]; downstream stages (flow dispatcher)
/// branch on it to broadcast virtual-time advance instead of flow-hashing.
#[derive(Debug, Clone)]
pub struct RawPacket {
    /// Capture timestamp in microseconds since Unix epoch.
    pub timestamp_us: i64,
    /// Number of bytes actually captured.
    pub caplen: u32,
    /// Original length on the wire.
    pub wirelen: u32,
    /// Link type from the pcap header (e.g., 1 = Ethernet, 101 = Raw IP).
    pub link_type: u32,
    /// Raw packet data starting at the link layer.
    pub data: Bytes,
    /// Identifies the logical source this packet belongs to. For pcap sources
    /// this is the interface name (or user-configured override); for cloud-probe
    /// it is the UUID extracted from the batch header.
    pub source_id: String,
}

impl RawPacket {
    /// True iff this packet is a heartbeat sentinel (Ethernet link-type,
    /// all-zero MACs, `ether_type == 0xFFFF`). Cheap: a prefix byte check.
    #[inline]
    pub fn is_heartbeat(&self) -> bool {
        // Only Ethernet II carries the sentinel; other link-types never do.
        if self.link_type != 1 {
            return false;
        }
        if self.data.len() < HEARTBEAT_PACKET_LEN {
            return false;
        }
        // First 12 bytes = dst MAC (6) + src MAC (6), all zero.
        if self.data[..12].iter().any(|b| *b != 0) {
            return false;
        }
        // Bytes 12..14 = ether_type, big-endian.
        let ether_type = u16::from_be_bytes([self.data[12], self.data[13]]);
        ether_type == HEARTBEAT_ETHER_TYPE
    }

    /// Build a synthetic heartbeat sentinel packet. Used by pcap-live when
    /// the interface is idle and no native heartbeat is available.
    pub fn heartbeat(timestamp_us: i64, source_id: String) -> Self {
        let mut buf = [0u8; HEARTBEAT_PACKET_LEN];
        buf[12] = 0xFF;
        buf[13] = 0xFF;
        RawPacket {
            timestamp_us,
            caplen: HEARTBEAT_PACKET_LEN as u32,
            wirelen: HEARTBEAT_PACKET_LEN as u32,
            link_type: 1,
            data: Bytes::copy_from_slice(&buf),
            source_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_roundtrip() {
        let hb = RawPacket::heartbeat(1_234_567, "test".to_string());
        assert!(hb.is_heartbeat());
        assert_eq!(hb.timestamp_us, 1_234_567);
        assert_eq!(hb.caplen, 14);
        assert_eq!(hb.link_type, 1);
    }

    #[test]
    fn real_packet_is_not_heartbeat() {
        let mut buf = [0u8; 14];
        buf[0] = 0xAA; // non-zero dst MAC
        buf[12] = 0x08;
        buf[13] = 0x00; // IPv4 ether_type
        let pkt = RawPacket {
            timestamp_us: 0,
            caplen: 14,
            wirelen: 14,
            link_type: 1,
            data: Bytes::copy_from_slice(&buf),
            source_id: String::new(),
        };
        assert!(!pkt.is_heartbeat());
    }

    #[test]
    fn zero_macs_but_wrong_ether_type_is_not_heartbeat() {
        let mut buf = [0u8; 14];
        buf[12] = 0x08;
        buf[13] = 0x00;
        let pkt = RawPacket {
            timestamp_us: 0,
            caplen: 14,
            wirelen: 14,
            link_type: 1,
            data: Bytes::copy_from_slice(&buf),
            source_id: String::new(),
        };
        assert!(!pkt.is_heartbeat());
    }

    #[test]
    fn non_ethernet_link_type_is_not_heartbeat() {
        let mut buf = [0u8; 14];
        buf[12] = 0xFF;
        buf[13] = 0xFF;
        let pkt = RawPacket {
            timestamp_us: 0,
            caplen: 14,
            wirelen: 14,
            link_type: 101, // Raw IP
            data: Bytes::copy_from_slice(&buf),
            source_id: String::new(),
        };
        assert!(!pkt.is_heartbeat());
    }
}
