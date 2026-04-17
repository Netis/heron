/// Errors that can occur while decoding a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ran out of bytes before the header was fully read.
    Truncated,
    /// The link type / protocol is not supported by this decoder.
    NotSupported,
    /// The packet is not an IP packet.
    NotIp,
    /// A header field contained an invalid or inconsistent value.
    InvalidHeader,
}

pub type DecodeResult<T> = Result<T, DecodeError>;
