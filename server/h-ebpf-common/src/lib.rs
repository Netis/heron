//! Shared event layout for the eBPF SSL-uprobe capture path.
//!
//! Defined once and used by both sides of the ring buffer:
//! * the BPF program (`h-ebpf-prog`, compiled for `bpfel-unknown-none`) writes
//!   these records, and
//! * the userspace loader (`h-capture`'s `ebpf` feature) reads them back.
//!
//! `#![no_std]` with no dependencies so it builds for the BPF target and the
//! host identically. The layout is `#[repr(C)]` and POD; the loader reads each
//! ring-buffer slice with an unaligned read, so no alignment guarantees are
//! required from the ring buffer.

#![no_std]

/// Length of a process `comm` (kernel `TASK_COMM_LEN`).
pub const COMM_LEN: usize = 16;

/// Maximum plaintext bytes carried in a single event. A single `SSL_read` /
/// `SSL_write` larger than this is split by the BPF program
/// (`h-ebpf-prog::emit_data`) into several consecutive same-direction events,
/// each carrying its absolute position in the connection-direction stream via
/// [`SslEvent::seq_off`], which the userspace synthesizer uses to place every
/// chunk at the correct sequence number (so a dropped/reordered chunk leaves a
/// detectable gap instead of shifting every later byte).
///
/// Sized at 32 KiB so a real-world Claude Code `/v1/messages` request — sent by
/// Node as ONE ~23 KiB `SSL_write` (request line + headers + JSON body) —
/// arrives whole in a single event. At the previous 4 KiB the request was cut
/// after its first 4 KiB, so `anthropic-version` / the JSON body were lost and
/// the wire-API registry could not recognize the call (it went to
/// `wires_ignored`), leaving every Claude Code call out of storage. The record
/// is reserved from the 16 MiB ring buffer (not the 512-byte BPF stack), so a
/// 32 KiB payload is fine, and the `bpf_probe_read_user` length stays clamped
/// to `DATA_CAP`, so the verifier can still prove the copy in-bounds.
pub const DATA_CAP: usize = 32768;

/// Event kind discriminants (`SslEvent::kind`).
pub mod kind {
    /// Plaintext written by the client (`SSL_write`) — client→server.
    pub const DATA_WRITE: u32 = 1;
    /// Plaintext read by the client (`SSL_read`) — server→client.
    pub const DATA_READ: u32 = 2;
    /// Connection torn down (`SSL_shutdown` / `SSL_free`).
    pub const CLOSE: u32 = 3;
}

/// One ring-buffer record. Fixed size (`DATA_CAP` payload) so the BPF program
/// can reserve it in the ring buffer and fill `data[..data_len]`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SslEvent {
    /// One of [`kind`].
    pub kind: u32,
    /// Userspace PID (thread-group id) that made the call.
    pub pid: u32,
    /// Per-connection handle: the `SSL*` pointer value, unique among live
    /// connections in a process. Identifies the logical connection.
    pub conn_id: u64,
    /// Kernel monotonic timestamp (`bpf_ktime_get_ns`).
    pub ktime_ns: u64,
    /// Absolute byte offset of `data[0]` within this connection-direction
    /// stream. The BPF program keeps a running per-`(conn_id, direction)`
    /// counter so that a single large `SSL_*` call split across several events —
    /// and successive calls on the same keep-alive connection — carry a
    /// monotonic position. The userspace synthesizer maps this to a TCP sequence
    /// number, so a silently dropped or reordered chunk leaves a gap at its true
    /// position instead of shifting every later byte earlier (which previously
    /// spliced the next request's bytes into the prior body). Always 0 for
    /// `CLOSE`.
    pub seq_off: u64,
    /// Valid bytes in `data` (0 for `CLOSE`).
    pub data_len: u32,
    /// Process name (`bpf_get_current_comm`), NUL-padded.
    pub comm: [u8; COMM_LEN],
    /// Plaintext payload, valid for `data_len` bytes.
    pub data: [u8; DATA_CAP],
}

impl SslEvent {
    /// Total size of the record on the wire.
    pub const SIZE: usize = core::mem::size_of::<Self>();
}
