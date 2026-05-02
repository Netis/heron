//! Byte encoders for the output `.pcap` stream. Mirror the writer-side
//! layout in `ts-capture::pcap_dump` byte-for-byte but live independently
//! per the no-dep rationale in the spec.

use crate::format::{PCAP_MAGIC, PCAP_SNAPLEN, PCAP_VERSION_MAJOR, PCAP_VERSION_MINOR};
use crate::reader::RawRec;

pub fn global_header(link_type: u32) -> [u8; 24] {
    let mut buf = [0u8; 24];
    buf[0..4].copy_from_slice(&PCAP_MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&PCAP_VERSION_MAJOR.to_le_bytes());
    buf[6..8].copy_from_slice(&PCAP_VERSION_MINOR.to_le_bytes());
    buf[8..12].copy_from_slice(&0i32.to_le_bytes());
    buf[12..16].copy_from_slice(&0u32.to_le_bytes());
    buf[16..20].copy_from_slice(&PCAP_SNAPLEN.to_le_bytes());
    buf[20..24].copy_from_slice(&link_type.to_le_bytes());
    buf
}

pub fn record_header(rec: &RawRec) -> [u8; 16] {
    let ts_sec = (rec.ts_us / 1_000_000) as u32;
    let ts_usec = (rec.ts_us % 1_000_000) as u32;
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&ts_sec.to_le_bytes());
    buf[4..8].copy_from_slice(&ts_usec.to_le_bytes());
    buf[8..12].copy_from_slice(&rec.caplen.to_le_bytes());
    buf[12..16].copy_from_slice(&rec.wirelen.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn global_header_layout() {
        let h = global_header(1);
        assert_eq!(&h[0..4], &PCAP_MAGIC.to_le_bytes());
        assert_eq!(&h[4..6], &2u16.to_le_bytes());
        assert_eq!(&h[6..8], &4u16.to_le_bytes());
        assert_eq!(&h[16..20], &PCAP_SNAPLEN.to_le_bytes());
        assert_eq!(&h[20..24], &1u32.to_le_bytes());
    }

    #[test]
    fn record_header_layout() {
        let rec = RawRec {
            ts_us: 1_500_250,
            caplen: 3,
            wirelen: 3,
            data: Bytes::from_static(&[0xaa, 0xbb, 0xcc]),
        };
        let h = record_header(&rec);
        assert_eq!(&h[0..4], &1u32.to_le_bytes());
        assert_eq!(&h[4..8], &500_250u32.to_le_bytes());
        assert_eq!(&h[8..12], &3u32.to_le_bytes());
        assert_eq!(&h[12..16], &3u32.to_le_bytes());
    }
}
