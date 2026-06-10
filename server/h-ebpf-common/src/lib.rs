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

/// Maximum plaintext bytes carried in a single event. A larger `SSL_read` /
/// `SSL_write` is emitted as several consecutive same-direction events, which
/// the userspace synthesizer concatenates by sequence number — so this cap
/// never loses bytes, it only bounds per-event size (BPF stack/verifier
/// limits make one giant copy impossible).
pub const DATA_CAP: usize = 4096;

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
