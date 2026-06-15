//! BPF program for eBPF SSL-uprobe capture.
//!
//! Attaches to OpenSSL / BoringSSL `SSL_write`, `SSL_read` and `SSL_shutdown`
//! and streams the plaintext (and connection lifecycle) to userspace over a
//! ring buffer as [`SslEvent`] records. The userspace loader (h-capture's
//! `EbpfSource`) turns those into synthetic TCP frames.
//!
//! Direction mapping: `SSL_write` ⇒ client→server (request), `SSL_read` ⇒
//! server→client (response). `SSL_read` reads its buffer only on return, so we
//! stash its args on entry and emit on the uretprobe using the real byte count.

#![no_std]
#![no_main]

use aya_ebpf::{
    helpers::{
        bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_ktime_get_ns,
        gen::bpf_probe_read_user,
    },
    macros::{map, uprobe, uretprobe},
    maps::{HashMap, RingBuf},
    programs::{ProbeContext, RetProbeContext},
};
use core::ffi::c_void;
use h_ebpf_common::{kind, SslEvent, DATA_CAP};

/// Ring buffer carrying [`SslEvent`]s to userspace (16 MiB).
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(16 * 1024 * 1024, 0);

/// `SSL_read` entry args stashed by tid until the matching uretprobe fires.
#[map]
static READ_ARGS: HashMap<u32, ReadArgs> = HashMap::with_max_entries(10240, 0);

#[repr(C)]
#[derive(Clone, Copy)]
struct ReadArgs {
    ssl: u64,
    buf: u64,
}

#[uprobe]
pub fn ssl_write(ctx: ProbeContext) -> u32 {
    let ssl: u64 = ctx.arg(0).unwrap_or(0);
    let buf: u64 = ctx.arg(1).unwrap_or(0);
    let num: i32 = ctx.arg(2).unwrap_or(0);
    if buf != 0 && num > 0 {
        emit_data(kind::DATA_WRITE, ssl, buf, num as u32);
    }
    0
}

#[uprobe]
pub fn ssl_read_enter(ctx: ProbeContext) -> u32 {
    let ssl: u64 = ctx.arg(0).unwrap_or(0);
    let buf: u64 = ctx.arg(1).unwrap_or(0);
    let tid = bpf_get_current_pid_tgid() as u32;
    let args = ReadArgs { ssl, buf };
    let _ = READ_ARGS.insert(&tid, &args, 0);
    0
}

#[uretprobe]
pub fn ssl_read_exit(ctx: RetProbeContext) -> u32 {
    let tid = bpf_get_current_pid_tgid() as u32;
    let args = match unsafe { READ_ARGS.get(&tid) } {
        Some(a) => *a,
        None => return 0,
    };
    let _ = READ_ARGS.remove(&tid);
    let ret: i32 = ctx.ret().unwrap_or(0);
    if args.buf != 0 && ret > 0 {
        emit_data(kind::DATA_READ, args.ssl, args.buf, ret as u32);
    }
    0
}

#[uprobe]
pub fn ssl_shutdown(ctx: ProbeContext) -> u32 {
    let ssl: u64 = ctx.arg(0).unwrap_or(0);
    emit_close(ssl);
    0
}

/// Max `DATA_CAP`-sized chunks emitted for one `SSL_*` call. A write larger than
/// `MAX_CHUNKS * DATA_CAP` (8 × 32 KiB = 256 KiB) loses its tail — rare, and the
/// userspace parser tolerates a body shorter than Content-Length.
const MAX_CHUNKS: u32 = 8;

/// Emit ONE `DATA_CAP`-sized chunk of `buf[start..]` if `start < len`.
///
/// `#[inline(always)]` + invoked from an UNROLLED sequence (not a loop) in
/// [`emit_data`] on purpose: a real loop's back-edge makes the 5.15 verifier
/// reject the program ("R1 type=ctx expected=fp"), and a non-inlined helper
/// makes it a BPF-to-BPF call the verifier also rejects. Inlined + unrolled, the
/// body is straight-line code — the exact single-event pattern that already
/// loaded — repeated `MAX_CHUNKS` times, which the verifier accepts.
#[inline(always)]
fn emit_chunk_at(ev_kind: u32, ssl: u64, buf: u64, start: u32, len: u32) {
    if start >= len {
        return;
    }
    let remaining = len - start;
    let n = if remaining as usize > DATA_CAP {
        DATA_CAP as u32
    } else {
        remaining
    };
    let mut entry = match EVENTS.reserve::<SslEvent>(0) {
        Some(e) => e,
        None => return,
    };
    let ev = entry.as_mut_ptr();
    unsafe {
        (*ev).kind = ev_kind;
        (*ev).pid = (bpf_get_current_pid_tgid() >> 32) as u32;
        (*ev).conn_id = ssl;
        (*ev).ktime_ns = bpf_ktime_get_ns();
        (*ev).data_len = n;
        if let Ok(comm) = bpf_get_current_comm() {
            (*ev).comm = comm;
        }
        let dst = core::ptr::addr_of_mut!((*ev).data) as *mut c_void;
        let _ = bpf_probe_read_user(dst, n, (buf + start as u64) as *const c_void);
    }
    entry.submit(0);
}

/// Stream a full `SSL_read`/`SSL_write` buffer to userspace as up to
/// [`MAX_CHUNKS`] consecutive `DATA_CAP`-sized events on the same `conn_id` +
/// direction.
///
/// Previously a call larger than `DATA_CAP` was truncated to a single event.
/// That broke HTTP framing on keep-alive connections: the request's
/// `Content-Length` (read from the intact header prefix) exceeded the captured
/// bytes, so the userspace parser kept reading the body PAST the truncated data
/// and swallowed the next request on the connection — corrupting both. Emitting
/// the WHOLE buffer in chunks lets the synthesizer turn each event into a
/// sequential TCP segment that reassembles into the complete plaintext, so
/// Content-Length matches and request boundaries stay correct.
///
/// Unrolled rather than looped: the 5.15 BPF verifier rejects the loop's
/// back-edge here. See [`emit_chunk_at`].
fn emit_data(ev_kind: u32, ssl: u64, buf: u64, len: u32) {
    let cap = DATA_CAP as u32;
    emit_chunk_at(ev_kind, ssl, buf, 0, len);
    emit_chunk_at(ev_kind, ssl, buf, cap, len);
    emit_chunk_at(ev_kind, ssl, buf, 2 * cap, len);
    emit_chunk_at(ev_kind, ssl, buf, 3 * cap, len);
    emit_chunk_at(ev_kind, ssl, buf, 4 * cap, len);
    emit_chunk_at(ev_kind, ssl, buf, 5 * cap, len);
    emit_chunk_at(ev_kind, ssl, buf, 6 * cap, len);
    emit_chunk_at(ev_kind, ssl, buf, 7 * cap, len);
}

fn emit_close(ssl: u64) {
    let mut entry = match EVENTS.reserve::<SslEvent>(0) {
        Some(e) => e,
        None => return,
    };
    let ev = entry.as_mut_ptr();
    unsafe {
        (*ev).kind = kind::CLOSE;
        (*ev).pid = (bpf_get_current_pid_tgid() >> 32) as u32;
        (*ev).conn_id = ssl;
        (*ev).ktime_ns = bpf_ktime_get_ns();
        (*ev).data_len = 0;
        if let Ok(comm) = bpf_get_current_comm() {
            (*ev).comm = comm;
        }
    }
    entry.submit(0);
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
