/// Errors that can occur while decoding a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ran out of bytes before the header was fully read.
    Truncated,
    /// L2 link type is not supported by this decoder.
    NotSupported,
    /// The packet is not an IP packet (non-IP L3 ethertype).
    NotIp,
    /// The packet is IP but the L4 protocol is not TCP.
    NotTcp,
    /// A header field contained an invalid or inconsistent value.
    InvalidHeader,
}

pub type DecodeResult<T> = Result<T, DecodeError>;
