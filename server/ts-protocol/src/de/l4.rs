use super::{
    buf::PacketBuf,
    error::{DecodeError, DecodeResult},
    headers::*,
    try_consume, try_skip,
};

/// L4 (TCP) information extracted from the packet header.
#[derive(Debug, PartialEq)]
pub struct L4Info {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    /// Length (bytes) of the TCP header itself (i.e. `data_offset * 4`).
    /// Combined with `L3Info::payload_length` this yields the on-wire TCP
    /// segment payload length even when capture is truncated.
    pub header_length: u32,
}

/// Dispatch L4 parsing based on `protocol` and advance the buffer past the
/// transport header. Returns `L4Info` with port numbers, sequence/ack numbers,
/// and TCP flags.
pub fn dispatch_l4(buf: &mut PacketBuf, protocol: u8) -> DecodeResult<L4Info> {
    match protocol {
        IP_PROTO_TCP => decode_tcp(buf),
        _ => Err(DecodeError::NotTcp),
    }
}

fn decode_tcp(buf: &mut PacketBuf) -> DecodeResult<L4Info> {
    let tcp = try_consume!(buf, TcpHeader);
    let data_offset = tcp.data_offset();
    if data_offset < 20 {
        return Err(DecodeError::InvalidHeader);
    }
    let src_port = tcp.src_port();
    let dst_port = tcp.dst_port();
    let seq = tcp.seq();
    let ack = tcp.ack();
    let flags = tcp.flags();
    if data_offset > 20 {
        try_skip!(buf, data_offset - 20);
    }
    Ok(L4Info {
        src_port,
        dst_port,
        seq,
        ack,
        flags,
        header_length: data_offset as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::de::buf::PacketBuf;

    fn make_tcp(src_port: u16, dst_port: u16, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
        let mut hdr = vec![0u8; 20];
        hdr[0..2].copy_from_slice(&src_port.to_be_bytes());
        hdr[2..4].copy_from_slice(&dst_port.to_be_bytes());
        hdr[4..8].copy_from_slice(&seq.to_be_bytes());
        hdr[8..12].copy_from_slice(&ack.to_be_bytes());
        hdr[12] = 0x50; // data_offset = 5 (5 * 4 = 20 bytes)
        hdr[13] = flags;
        hdr
    }

    #[test]
    fn tcp_standard() {
        let mut data = make_tcp(12345, 80, 100, 200, 0x12);
        data.extend_from_slice(b"payload");
        let mut buf = PacketBuf::new(&data);
        let info = dispatch_l4(&mut buf, IP_PROTO_TCP).unwrap();
        assert_eq!(info.src_port, 12345);
        assert_eq!(info.dst_port, 80);
        assert_eq!(info.seq, 100);
        assert_eq!(info.ack, 200);
        assert_eq!(info.flags, 0x12);
        assert_eq!(buf.remaining_slice(), b"payload");
    }

    #[test]
    fn tcp_with_options() {
        let mut hdr = vec![0u8; 20];
        hdr[0..2].copy_from_slice(&1234u16.to_be_bytes());
        hdr[2..4].copy_from_slice(&5678u16.to_be_bytes());
        hdr[4..8].copy_from_slice(&1u32.to_be_bytes());
        hdr[8..12].copy_from_slice(&2u32.to_be_bytes());
        hdr[12] = 0x80; // data_offset = 8 (8 * 4 = 32 bytes)
        hdr[13] = 0x10;
        // 12 bytes of options
        let mut data = hdr;
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(b"data");
        let mut buf = PacketBuf::new(&data);
        let info = dispatch_l4(&mut buf, IP_PROTO_TCP).unwrap();
        assert_eq!(buf.offset(), 32);
        assert_eq!(buf.remaining_slice(), b"data");
        assert_eq!(info.src_port, 1234);
    }

    #[test]
    fn tcp_invalid_data_offset() {
        let mut hdr = vec![0u8; 20];
        hdr[12] = 0x30; // data_offset = 3 (3 * 4 = 12 bytes, invalid)
        let mut buf = PacketBuf::new(&hdr);
        let result = dispatch_l4(&mut buf, IP_PROTO_TCP);
        assert_eq!(result, Err(DecodeError::InvalidHeader));
    }

    #[test]
    fn tcp_truncated() {
        let data = vec![0u8; 10];
        let mut buf = PacketBuf::new(&data);
        let result = dispatch_l4(&mut buf, IP_PROTO_TCP);
        assert_eq!(result, Err(DecodeError::Truncated));
    }

    #[test]
    fn tcp_options_truncated() {
        let mut hdr = vec![0u8; 20];
        hdr[12] = 0x80; // data_offset = 8 (32 bytes total), but no option bytes follow
        let mut buf = PacketBuf::new(&hdr);
        let result = dispatch_l4(&mut buf, IP_PROTO_TCP);
        assert_eq!(result, Err(DecodeError::Truncated));
    }

    #[test]
    fn udp_not_supported() {
        let data = vec![0u8; 20];
        let mut buf = PacketBuf::new(&data);
        let result = dispatch_l4(&mut buf, 17);
        assert_eq!(result, Err(DecodeError::NotTcp));
    }

    #[test]
    fn tcp_no_payload() {
        let hdr = make_tcp(8080, 443, 0, 0, 0x10); // ACK only
        let mut buf = PacketBuf::new(&hdr);
        let info = dispatch_l4(&mut buf, IP_PROTO_TCP).unwrap();
        assert_eq!(info.flags, 0x10);
        assert_eq!(buf.remaining(), 0);
    }
}
