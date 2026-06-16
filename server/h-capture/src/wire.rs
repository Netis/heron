//! Probe↔central wire protocol (v1) for distributed eBPF capture.
//!
//! A thin `heron-probe` on an edge host does eBPF SSL-uprobe capture and frame
//! synthesis, then ships the resulting [`RawPacket`]s — process attribution and
//! all — to a central `heron` over an mTLS stream. This module owns the *pure*
//! codec: how a batch of packets becomes the bytes inside one length-delimited
//! frame, and back. It does no I/O — the transport (mTLS + length framing) lives
//! in the probe binary and [`crate::thin_probe`]; both build their codec via
//! [`length_delimited_codec`] so they can't drift on max frame length.
//!
//! Frame payload layout (the bytes inside one length-delimited frame):
//!
//! ```text
//! [version: u8] ++ postcard(ProbeBatch)
//! ```
//!
//! The version byte sits *outside* the postcard blob on purpose: the decoder
//! checks it before handing bytes to a possibly-incompatible schema, so a
//! version skew fails loudly with [`WireError::UnsupportedVersion`] instead of a
//! confusing mid-struct decode error. This is the deliberate fix for the bug
//! class where cloud-probe's batch `version` field is parsed but never validated
//! (`docs/design/02-capture.md`): here the version is checked from frame one.

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::codec::LengthDelimitedCodec;

use crate::packet::RawPacket;

/// Current probe↔central protocol version. Bump on any incompatible change to
/// [`ProbeBatch`] or the frame layout; the decoder rejects any other value.
pub const PROTOCOL_VERSION: u8 = 1;

/// Max frame size the central accepts from a probe. One `SSL_write` event is at
/// most the uprobe segment (~32 KiB) and a batch bundles many, so real frames
/// are well under this — it exists to bound a hostile or corrupt length prefix
/// before any allocation. Both ends share it via [`length_delimited_codec`].
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// One batch of captured packets from a single probe — the unit shipped per
/// length-delimited frame.
///
/// `source_id` is the probe's authoritative identity (its configured id, or —
/// when left empty — the central fills it from the client-certificate CN). It is
/// stamped onto every packet on arrival, mirroring how `cloud_probe` stamps its
/// batch UUID onto each `RawPacket`. Per-packet `source_id` set probe-side is
/// therefore advisory and overwritten centrally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeBatch {
    /// The probe's identity for this batch (may be empty → resolved from cert CN).
    pub source_id: String,
    /// Captured packets in capture order, process attribution included.
    pub packets: Vec<RawPacket>,
}

impl ProbeBatch {
    /// Build a batch tagged with `source_id`.
    pub fn new(source_id: impl Into<String>, packets: Vec<RawPacket>) -> Self {
        Self {
            source_id: source_id.into(),
            packets,
        }
    }
}

/// Failure encoding or decoding a wire frame. On a decode failure the caller
/// drops the whole frame (same coarse-grained policy as `cloud_probe`'s
/// drop-the-batch), counting it so a misbehaving probe is visible.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("empty frame (missing version byte)")]
    Empty,
    #[error("unsupported protocol version {got} (this build speaks {expected})")]
    UnsupportedVersion { got: u8, expected: u8 },
    #[error("postcard decode failed: {0}")]
    Decode(#[from] postcard::Error),
    #[error("postcard encode failed: {0}")]
    Encode(postcard::Error),
}

/// Encode a batch into one frame payload: `[version] ++ postcard(batch)`. The
/// caller wraps the returned bytes in a length-delimited frame for the stream.
pub fn encode_frame(batch: &ProbeBatch) -> Result<Bytes, WireError> {
    let body = postcard::to_allocvec(batch).map_err(WireError::Encode)?;
    let mut buf = BytesMut::with_capacity(1 + body.len());
    buf.put_u8(PROTOCOL_VERSION);
    buf.extend_from_slice(&body);
    Ok(buf.freeze())
}

/// Decode one frame payload (`[version] ++ postcard`) back into a [`ProbeBatch`],
/// validating the leading version byte before touching the schema-versioned body.
pub fn decode_frame(frame: &[u8]) -> Result<ProbeBatch, WireError> {
    let (&version, body) = frame.split_first().ok_or(WireError::Empty)?;
    if version != PROTOCOL_VERSION {
        return Err(WireError::UnsupportedVersion {
            got: version,
            expected: PROTOCOL_VERSION,
        });
    }
    Ok(postcard::from_bytes(body)?)
}

/// The length-delimited codec both ends use to frame [`encode_frame`] payloads
/// on the mTLS stream. Centralized so the probe and the central can't disagree
/// on `max_frame_length`.
pub fn length_delimited_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use h_common::process::ProcessInfo;

    fn sample_packet(ts: i64, payload: &[u8], process: Option<ProcessInfo>) -> RawPacket {
        RawPacket {
            timestamp_us: ts,
            caplen: payload.len() as u32,
            wirelen: payload.len() as u32,
            link_type: 1,
            data: Bytes::copy_from_slice(payload),
            source_id: "ebpf".to_string(),
            process,
        }
    }

    /// A full batch — including a packet carrying `ProcessInfo` with pid/comm/exe
    /// — survives encode→decode byte-for-byte. This is the core data-contract
    /// guarantee: the central sees exactly what the probe captured.
    #[test]
    fn roundtrip_preserves_process_attribution() {
        let proc = ProcessInfo::new(4242, "claude").with_exe(Some("/usr/bin/claude".into()));
        let batch = ProbeBatch::new(
            "gateway-1",
            vec![
                sample_packet(1_000_000, &[0xde, 0xad, 0xbe, 0xef], Some(proc.clone())),
                sample_packet(1_000_500, &[0x01, 0x02], None),
            ],
        );

        let frame = encode_frame(&batch).expect("encode");
        let decoded = decode_frame(&frame).expect("decode");

        assert_eq!(decoded, batch);
        // Spell out the attribution assertion the plan calls for.
        let p0 = &decoded.packets[0].process;
        assert_eq!(p0.as_ref().unwrap().pid, 4242);
        assert_eq!(p0.as_ref().unwrap().comm, "claude");
        assert_eq!(p0.as_ref().unwrap().exe.as_deref(), Some("/usr/bin/claude"));
        assert!(decoded.packets[1].process.is_none());
    }

    /// The raw `data` bytes must come back identical — no truncation, no
    /// re-encoding of the captured plaintext.
    #[test]
    fn roundtrip_preserves_packet_bytes() {
        let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let batch = ProbeBatch::new("p", vec![sample_packet(7, &payload, None)]);
        let decoded = decode_frame(&encode_frame(&batch).unwrap()).unwrap();
        assert_eq!(&decoded.packets[0].data[..], &payload[..]);
    }

    /// An empty batch (a probe with nothing to report) is a valid frame.
    #[test]
    fn roundtrip_empty_batch() {
        let batch = ProbeBatch::new("p", vec![]);
        let decoded = decode_frame(&encode_frame(&batch).unwrap()).unwrap();
        assert_eq!(decoded, batch);
    }

    /// A frame from a newer/older protocol is rejected up front with the
    /// observed and expected versions — the explicit cloud-probe-bug fix.
    #[test]
    fn version_mismatch_is_rejected() {
        let batch = ProbeBatch::new("p", vec![sample_packet(1, &[0xaa], None)]);
        let mut frame = encode_frame(&batch).unwrap().to_vec();
        frame[0] = PROTOCOL_VERSION.wrapping_add(7); // pretend a future version
        match decode_frame(&frame) {
            Err(WireError::UnsupportedVersion { got, expected }) => {
                assert_eq!(got, PROTOCOL_VERSION.wrapping_add(7));
                assert_eq!(expected, PROTOCOL_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    /// A zero-length frame has no version byte and is rejected, not panicked on.
    #[test]
    fn empty_frame_is_rejected() {
        assert!(matches!(decode_frame(&[]), Err(WireError::Empty)));
    }

    /// A correct version byte followed by garbage must surface as a decode error,
    /// never a panic — a hostile or corrupt probe can't wedge the central.
    #[test]
    fn garbage_body_is_a_decode_error() {
        // 0xFF leads with a huge varint string length → postcard runs out of input.
        let frame = [PROTOCOL_VERSION, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(matches!(decode_frame(&frame), Err(WireError::Decode(_))));
    }
}
