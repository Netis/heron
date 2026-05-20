//! Per-record predicate: time window AND 5-tuple in either direction.
//! Records that don't decode to TCP via `ts_protocol::de::decode` are
//! dropped. (TokenScope is TCP-only today; ARP / ICMP / UDP / QUIC /
//! malformed all fall through to "skip".)

use ts_protocol::de::decode;

use crate::reader::RawRec;
use crate::types::ExtractRequestSet;

pub struct Filter<'a> {
    pub req: &'a ExtractRequestSet,
    pub link_type: u32,
}

impl Filter<'_> {
    pub fn matches(&self, rec: &RawRec) -> bool {
        if rec.ts_us < self.req.start_us || rec.ts_us > self.req.end_us {
            return false;
        }
        // ts_protocol::de::decode wants a source_id + ts_us only for diagnostic
        // bookkeeping in the parsed value; we reuse the request's source_id.
        let parsed = match decode(
            &rec.data,
            self.link_type,
            rec.ts_us,
            self.req.source_id.clone(),
        ) {
            Ok(p) => p,
            Err(_) => return false,
        };
        self.req.flows.iter().any(|flow| {
            if rec.ts_us < flow.start_us || rec.ts_us > flow.end_us {
                return false;
            }
            let forward = field_match(flow.client_ip, parsed.src_ip)
                && field_match(flow.client_port, parsed.src_port)
                && field_match(flow.server_ip, parsed.dst_ip)
                && field_match(flow.server_port, parsed.dst_port);
            let reverse = field_match(flow.client_ip, parsed.dst_ip)
                && field_match(flow.client_port, parsed.dst_port)
                && field_match(flow.server_ip, parsed.src_ip)
                && field_match(flow.server_port, parsed.src_port);
            forward || reverse
        })
    }
}

fn field_match<T: Eq>(filter: Option<T>, actual: T) -> bool {
    filter.is_none_or(|f| f == actual)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;
    use crate::types::ExtractFlow;
    use bytes::Bytes;

    /// Build a minimal Ethernet+IPv4+TCP packet (20+20 byte headers) with the
    /// given addresses. Caplen == frame length.
    fn ipv4_tcp_pkt(src_ip: [u8; 4], src_port: u16, dst_ip: [u8; 4], dst_port: u16) -> Vec<u8> {
        let mut frame = Vec::new();
        // Ethernet (14): dst mac + src mac + ethertype 0x0800
        frame.extend_from_slice(&[0u8; 6]);
        frame.extend_from_slice(&[0u8; 6]);
        frame.extend_from_slice(&[0x08, 0x00]);
        // IPv4 (20)
        let ip_total_len: u16 = 20 + 20;
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45; // version 4, IHL 5
        ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
        ip[8] = 64; // TTL
        ip[9] = 6; // protocol = TCP
        ip[12..16].copy_from_slice(&src_ip);
        ip[16..20].copy_from_slice(&dst_ip);
        frame.extend_from_slice(&ip);
        // TCP (20)
        let mut tcp = vec![0u8; 20];
        tcp[0..2].copy_from_slice(&src_port.to_be_bytes());
        tcp[2..4].copy_from_slice(&dst_port.to_be_bytes());
        tcp[12] = 0x50; // data offset 5 (no options), reserved 0
        tcp[13] = 0x10; // ACK
        frame.extend_from_slice(&tcp);
        frame
    }

    fn rec_for(data: Vec<u8>, ts_us: i64) -> RawRec {
        let len = data.len() as u32;
        RawRec {
            ts_us,
            caplen: len,
            wirelen: len,
            data: Bytes::from(data),
        }
    }

    fn flow_with(
        start_us: i64,
        end_us: i64,
        client_ip: Option<IpAddr>,
        client_port: Option<u16>,
        server_ip: Option<IpAddr>,
        server_port: Option<u16>,
    ) -> ExtractFlow {
        ExtractFlow {
            start_us,
            end_us,
            client_ip,
            client_port,
            server_ip,
            server_port,
        }
    }

    #[test]
    fn matches_forward_direction() {
        let pkt = ipv4_tcp_pkt([10, 0, 0, 1], 54321, [1, 2, 3, 4], 443);
        let req = ExtractRequestSet {
            source_id: "test".into(),
            start_us: 0,
            end_us: 10_000_000,
            flows: vec![flow_with(
                0,
                10_000_000,
                "10.0.0.1".parse().ok(),
                Some(54321),
                "1.2.3.4".parse().ok(),
                Some(443),
            )],
        };
        let f = Filter {
            req: &req,
            link_type: 1,
        };
        assert!(f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn matches_reverse_direction() {
        // Packet flowing server → client; same filter still matches.
        let pkt = ipv4_tcp_pkt([1, 2, 3, 4], 443, [10, 0, 0, 1], 54321);
        let req = ExtractRequestSet {
            source_id: "test".into(),
            start_us: 0,
            end_us: 10_000_000,
            flows: vec![flow_with(
                0,
                10_000_000,
                "10.0.0.1".parse().ok(),
                Some(54321),
                "1.2.3.4".parse().ok(),
                Some(443),
            )],
        };
        let f = Filter {
            req: &req,
            link_type: 1,
        };
        assert!(f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn empty_fields_are_wildcard() {
        let pkt = ipv4_tcp_pkt([10, 0, 0, 1], 54321, [1, 2, 3, 4], 443);
        let req = ExtractRequestSet {
            source_id: "test".into(),
            start_us: 0,
            end_us: 10_000_000,
            flows: vec![flow_with(0, 10_000_000, None, None, None, None)],
        };
        let f = Filter {
            req: &req,
            link_type: 1,
        };
        assert!(f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn rejects_outside_time_window() {
        let pkt = ipv4_tcp_pkt([10, 0, 0, 1], 54321, [1, 2, 3, 4], 443);
        let req = ExtractRequestSet {
            source_id: "test".into(),
            start_us: 2_000_000,
            end_us: 5_000_000,
            flows: vec![flow_with(2_000_000, 5_000_000, None, None, None, None)],
        };
        let f = Filter {
            req: &req,
            link_type: 1,
        };
        assert!(!f.matches(&rec_for(pkt.clone(), 1_999_999)));
        assert!(f.matches(&rec_for(pkt.clone(), 2_000_000))); // inclusive lo
        assert!(f.matches(&rec_for(pkt.clone(), 5_000_000))); // inclusive hi
        assert!(!f.matches(&rec_for(pkt, 5_000_001)));
    }

    #[test]
    fn drops_non_matching_5_tuple() {
        let pkt = ipv4_tcp_pkt([10, 0, 0, 1], 54321, [1, 2, 3, 4], 443);
        let req = ExtractRequestSet {
            source_id: "test".into(),
            start_us: 0,
            end_us: 10_000_000,
            flows: vec![flow_with(
                0,
                10_000_000,
                "10.0.0.99".parse().ok(),
                None,
                None,
                None,
            )],
        }; // wrong client_ip
        let f = Filter {
            req: &req,
            link_type: 1,
        };
        assert!(!f.matches(&rec_for(pkt, 1_000_000)));
    }

    #[test]
    fn drops_non_tcp_packet() {
        // Pure Ethernet, no IPv4: ts_protocol::de::decode returns Err.
        let frame = vec![0u8; 14];
        let req = ExtractRequestSet {
            source_id: "test".into(),
            start_us: 0,
            end_us: 10_000_000,
            flows: vec![flow_with(0, 10_000_000, None, None, None, None)],
        };
        let f = Filter {
            req: &req,
            link_type: 1,
        };
        assert!(!f.matches(&rec_for(frame, 1_000_000)));
    }

    #[test]
    fn matches_any_configured_flow_but_honors_flow_window() {
        let pkt_a = ipv4_tcp_pkt([10, 0, 0, 1], 11111, [1, 2, 3, 4], 443);
        let pkt_b = ipv4_tcp_pkt([10, 0, 0, 1], 22222, [1, 2, 3, 4], 443);
        let req = ExtractRequestSet {
            source_id: "test".into(),
            start_us: 0,
            end_us: 20_000_000,
            flows: vec![
                flow_with(
                    0,
                    5_000_000,
                    "10.0.0.1".parse().ok(),
                    Some(11111),
                    "1.2.3.4".parse().ok(),
                    Some(443),
                ),
                flow_with(
                    10_000_000,
                    20_000_000,
                    "10.0.0.1".parse().ok(),
                    Some(22222),
                    "1.2.3.4".parse().ok(),
                    Some(443),
                ),
            ],
        };
        let f = Filter {
            req: &req,
            link_type: 1,
        };
        assert!(f.matches(&rec_for(pkt_a.clone(), 1_000_000)));
        assert!(!f.matches(&rec_for(pkt_a, 12_000_000)));
        assert!(f.matches(&rec_for(pkt_b, 12_000_000)));
    }
}
