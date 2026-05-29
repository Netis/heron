use std::hash::{Hash, Hasher};
use std::net::IpAddr;

use bytes::Bytes;

/// Identifies a TCP connection. Normalized so the smaller (IP, port) pair is
/// always `addr_a`, ensuring both directions hash to the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowKey {
    pub source_id: String,
    pub addr_a: (IpAddr, u16),
    pub addr_b: (IpAddr, u16),
}

impl FlowKey {
    pub fn new(
        source_id: String,
        src_ip: IpAddr,
        src_port: u16,
        dst_ip: IpAddr,
        dst_port: u16,
    ) -> Self {
        let a = (src_ip, src_port);
        let b = (dst_ip, dst_port);
        if (src_ip, src_port) <= (dst_ip, dst_port) {
            Self {
                source_id,
                addr_a: a,
                addr_b: b,
            }
        } else {
            Self {
                source_id,
                addr_a: b,
                addr_b: a,
            }
        }
    }

    /// Returns a hash suitable for flow sharding.
    pub fn shard_hash(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl Hash for FlowKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source_id.hash(state);
        self.addr_a.0.hash(state);
        self.addr_a.1.hash(state);
        self.addr_b.0.hash(state);
        self.addr_b.1.hash(state);
    }
}

impl std::fmt::Display for FlowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {}:{} <-> {}:{}",
            self.source_id, self.addr_a.0, self.addr_a.1, self.addr_b.0, self.addr_b.1
        )
    }
}

/// Which direction the packet is going relative to the normalized FlowKey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From addr_a to addr_b (same order as FlowKey normalization).
    AtoB,
    /// From addr_b to addr_a.
    BtoA,
}

/// A parsed network packet with extracted TCP fields.
#[derive(Debug, Clone)]
pub struct ParsedPacket {
    pub flow_key: FlowKey,
    pub direction: Direction,
    pub src_ip: IpAddr,
    pub src_port: u16,
    pub dst_ip: IpAddr,
    pub dst_port: u16,
    pub tcp_flags: u8,
    pub tcp_seq: u32,
    pub tcp_ack: u32,
    /// Captured TCP segment bytes. May be shorter than [`Self::wire_payload_len`]
    /// when the capture was snaplen-truncated.
    pub payload: Bytes,
    /// On-wire TCP segment payload length (bytes), derived from the IP and TCP
    /// header fields rather than `payload.len()`. The reassembler uses this
    /// for sequence-number math so that snaplen truncation does not silently
    /// desynchronize the per-direction byte stream.
    pub wire_payload_len: u32,
    pub timestamp_us: i64,
}

// TCP flag constants.
pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;

impl ParsedPacket {
    pub fn has_syn(&self) -> bool {
        self.tcp_flags & TCP_SYN != 0
    }
    pub fn has_fin(&self) -> bool {
        self.tcp_flags & TCP_FIN != 0
    }
    pub fn has_rst(&self) -> bool {
        self.tcp_flags & TCP_RST != 0
    }
    pub fn has_ack(&self) -> bool {
        self.tcp_flags & TCP_ACK != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flow_key_normalization() {
        let ip_a: IpAddr = "10.0.0.1".parse().unwrap();
        let ip_b: IpAddr = "10.0.0.2".parse().unwrap();

        let fk1 = FlowKey::new(String::new(), ip_a, 1000, ip_b, 80);
        let fk2 = FlowKey::new(String::new(), ip_b, 80, ip_a, 1000);

        assert_eq!(fk1, fk2);
        assert_eq!(fk1.shard_hash(), fk2.shard_hash());
    }

    #[test]
    fn test_direction() {
        let ip_a: IpAddr = "10.0.0.1".parse().unwrap();
        let ip_b: IpAddr = "10.0.0.2".parse().unwrap();

        let fk = FlowKey::new(String::new(), ip_a, 1000, ip_b, 80);
        assert_eq!(fk.addr_a, (ip_a, 1000));
        assert_eq!(fk.addr_b, (ip_b, 80));
    }

    #[test]
    fn flow_key_different_source_id_not_equal() {
        let ip_a: IpAddr = "10.0.0.1".parse().unwrap();
        let ip_b: IpAddr = "10.0.0.2".parse().unwrap();
        let fk1 = FlowKey::new("s1".to_string(), ip_a, 1000, ip_b, 80);
        let fk2 = FlowKey::new("s2".to_string(), ip_a, 1000, ip_b, 80);
        assert_ne!(fk1, fk2);
        assert_ne!(fk1.shard_hash(), fk2.shard_hash());
    }
}
