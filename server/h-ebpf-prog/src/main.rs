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

/// Reserve a ring-buffer record, fill the header, and copy up to [`DATA_CAP`]
/// plaintext bytes from the userspace buffer. A larger call is truncated to
/// `DATA_CAP` here; the userspace synthesizer treats each event as a contiguous
/// segment, so consecutive events on the same connection reassemble in order.
fn emit_data(ev_kind: u32, ssl: u64, buf: u64, len: u32) {
    let n = if len as usize > DATA_CAP {
        DATA_CAP as u32
    } else {
        len
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
        let _ = bpf_probe_read_user(dst, n, buf as *const c_void);
    }
    entry.submit(0);
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
