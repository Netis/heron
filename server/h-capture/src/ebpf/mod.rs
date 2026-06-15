//! eBPF SSL-uprobe capture source (Linux only).
//!
//! An eBPF program attached to `SSL_read` / `SSL_write` (Phase 1: dynamically
//! linked OpenSSL / BoringSSL) hands userspace the plaintext of TLS-encrypted
//! traffic — per connection, per direction — before/after the library
//! encrypts it. That lets Heron observe LLM API calls to external providers
//! (e.g. `api.anthropic.com`) on the host, with no proxy in the request path.
//!
//! This module is split into:
//! * a **cross-platform core** — [`SslEvent`], [`BootClock`], [`EbpfPump`] —
//!   that turns a stream of decoded SSL events into synthetic [`RawPacket`]s
//!   via [`FlowSynthesizer`](crate::synth::FlowSynthesizer), and
//! * a **Linux-only loader** ([`EbpfSource`]) that loads the BPF programs,
//!   attaches the uprobes, polls the ring buffer, decodes raw events, and
//!   feeds them to the pump.
//!
//! The pump is the userspace half's only non-trivial logic, so it lives in the
//! cross-platform core and is unit-tested on every platform. The loader is a
//! thin, Linux-gated shell around it.

use bytes::Bytes;

use h_common::process::ProcessInfo;

use crate::heartbeat::HeartbeatTracker;
use crate::packet::RawPacket;
use crate::synth::{ConnTuple, FlowSynthesizer, StreamDir, SynthConfig};

// The aya loader (`source.rs`, the `EbpfSource` CaptureSource impl) pulls in
// Linux-only native dependencies and the off-tree BPF program. It is gated
// behind both the target OS and the off-by-default `ebpf` cargo feature, so
// default builds — macOS dev and the Linux CI alike — never compile it and stay
// green. Enable with `--features ebpf` on a Linux host (CAP_BPF + BTF) to build
// real capture. Landed in Phase 1b; until then the capture factory returns a
// clear "not available" error for `ebpf` sources.
#[cfg(all(target_os = "linux", feature = "ebpf"))]
mod source;
#[cfg(all(target_os = "linux", feature = "ebpf"))]
pub use source::EbpfSource;

// Byte-signature scanning for offset-based uprobe attach on symbol-stripped
// static TLS stacks (Bun/BoringSSL, Phase 3). Pure + cross-platform so the
// matcher is built and unit-tested on every host, like `synth`.
pub mod sigscan;

/// Maximum length of a process `comm` (matches the kernel `TASK_COMM_LEN`).
pub const COMM_LEN: usize = 16;

/// A decoded event from the BPF ring buffer.
///
/// `conn_id` is a stable per-connection handle (the `SSL*` pointer value, which
/// is unique among live connections in a process); combined with the pid it
/// uniquely identifies one TLS connection. `ktime_ns` is the kernel monotonic
/// timestamp (`bpf_ktime_get_ns`), converted to wall-clock by [`BootClock`].
#[derive(Debug, Clone)]
pub enum SslEvent {
    /// A connection was established (`SSL_set_fd` / first activity). Carries the
    /// real socket 5-tuple when recovered (connect kprobe), else `None` and the
    /// pump synthesizes a stable placeholder tuple.
    Connect {
        conn_id: u64,
        pid: u32,
        comm: String,
        /// Best-effort absolute executable path, resolved in userspace by the
        /// loader (`/proc/<pid>/exe`). `None` when unresolved.
        exe: Option<String>,
        tuple: Option<ConnTuple>,
        ktime_ns: u64,
    },
    /// Plaintext bytes observed in one direction (`SSL_write` ⇒
    /// [`StreamDir::ClientToServer`], `SSL_read` ⇒
    /// [`StreamDir::ServerToClient`]).
    Data {
        conn_id: u64,
        pid: u32,
        comm: String,
        /// Best-effort absolute executable path (see [`SslEvent::Connect`]).
        exe: Option<String>,
        dir: StreamDir,
        data: Bytes,
        /// Absolute byte offset of `data[0]` within this connection-direction
        /// stream (BPF per-connection counter). Lets the synthesizer place a
        /// chunk at its true sequence even after a dropped/reordered sibling.
        seq_off: u64,
        ktime_ns: u64,
    },
    /// The connection was torn down (`SSL_shutdown` / `close`).
    Close { conn_id: u64, ktime_ns: u64 },
}

impl SslEvent {
    fn pid(&self) -> Option<u32> {
        match self {
            SslEvent::Connect { pid, .. } | SslEvent::Data { pid, .. } => Some(*pid),
            SslEvent::Close { .. } => None,
        }
    }

    /// Process attribution carried by this event, if any. `Connect`/`Data`
    /// events carry the owning process (pid + comm + best-effort exe); `Close`
    /// events do not (the FIN frames they synthesize carry no HTTP payload, so
    /// the flow has already learned the process from an earlier data frame).
    fn process(&self) -> Option<ProcessInfo> {
        match self {
            SslEvent::Connect {
                pid, comm, exe, ..
            }
            | SslEvent::Data {
                pid, comm, exe, ..
            } => Some(ProcessInfo {
                pid: *pid,
                comm: comm.clone(),
                exe: exe.clone(),
            }),
            SslEvent::Close { .. } => None,
        }
    }

    fn ktime_ns(&self) -> u64 {
        match self {
            SslEvent::Connect { ktime_ns, .. }
            | SslEvent::Data { ktime_ns, .. }
            | SslEvent::Close { ktime_ns, .. } => *ktime_ns,
        }
    }
}

/// Converts a kernel monotonic timestamp (`bpf_ktime_get_ns`, nanoseconds since
/// boot) into Unix-epoch microseconds, which is what [`RawPacket::timestamp_us`]
/// requires. The offset between the two clocks is sampled once at construction.
#[derive(Debug, Clone, Copy)]
pub struct BootClock {
    /// `epoch_us - monotonic_us`, added to every converted timestamp.
    offset_us: i64,
}

impl BootClock {
    /// Build a clock from an explicit offset. Used by tests and as the building
    /// block for [`from_system`](Self::from_system).
    pub fn with_offset_us(offset_us: i64) -> Self {
        Self { offset_us }
    }

    /// Sample the live `CLOCK_REALTIME - CLOCK_MONOTONIC` offset (Linux). The
    /// monotonic clock is the same source as `bpf_ktime_get_ns`.
    #[cfg(target_os = "linux")]
    pub fn from_system() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        // SAFETY: clock_gettime with a valid clock id and timespec out-param.
        let mono_ns = {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
            ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
        };
        let epoch_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Self {
            offset_us: (epoch_ns - mono_ns) / 1_000,
        }
    }

    /// Convert a `bpf_ktime_get_ns` value to Unix-epoch microseconds.
    pub fn ktime_to_epoch_us(&self, ktime_ns: u64) -> i64 {
        (ktime_ns / 1_000) as i64 + self.offset_us
    }
}

/// Drives a [`FlowSynthesizer`] and [`HeartbeatTracker`] from a stream of
/// [`SslEvent`]s, producing the [`RawPacket`]s to forward downstream.
///
/// This is the testable heart of the eBPF userspace path. The Linux loader does
/// only IO around it: poll the ring buffer, decode bytes into [`SslEvent`],
/// call [`on_event`](Self::on_event), and send the returned packets.
#[derive(Debug)]
pub struct EbpfPump {
    synth: FlowSynthesizer,
    heartbeat: HeartbeatTracker,
    clock: BootClock,
    /// When non-empty, only these PIDs are captured.
    pid_allowlist: Vec<u32>,
}

impl EbpfPump {
    pub fn new(cfg: SynthConfig, clock: BootClock, pid_allowlist: Vec<u32>) -> Self {
        Self {
            synth: FlowSynthesizer::new(cfg),
            heartbeat: HeartbeatTracker::new(),
            clock,
            pid_allowlist,
        }
    }

    /// Number of live connections currently tracked (for metrics/observability).
    pub fn conn_count(&self) -> usize {
        self.synth.conn_count()
    }

    /// Process one decoded SSL event and return the [`RawPacket`]s to forward,
    /// in order. Each data/handshake/FIN frame is preceded by a synthesized
    /// heartbeat when ≥1 s of event-time has elapsed since the last one, so the
    /// downstream stages' time-driven windows advance even on sparse traffic.
    pub fn on_event(&mut self, ev: SslEvent) -> Vec<RawPacket> {
        // PID allowlist: drop events from processes we are not capturing. Close
        // events have no pid; let them through so connections still finalize.
        if !self.pid_allowlist.is_empty() {
            if let Some(pid) = ev.pid() {
                if !self.pid_allowlist.contains(&pid) {
                    return Vec::new();
                }
            }
        }

        let ts_us = self.clock.ktime_to_epoch_us(ev.ktime_ns());
        // Snapshot the owning process before the match consumes `ev`. Stamped
        // onto every synthesized data/handshake frame below so the flow learns
        // the attribution; `Close` carries no process (its FINs need none).
        let process = ev.process();
        let frames = match ev {
            SslEvent::Connect {
                conn_id, tuple, ..
            } => {
                let tuple = tuple.unwrap_or_else(|| FlowSynthesizer::synthetic_tuple(conn_id));
                self.synth.open(conn_id, tuple, ts_us)
            }
            SslEvent::Data {
                conn_id,
                dir,
                data,
                seq_off,
                ..
            } => self.synth.data(conn_id, dir, &data, seq_off, ts_us),
            SslEvent::Close { conn_id, .. } => self.synth.close(conn_id, ts_us),
        };

        // Interleave heartbeats ahead of the frames they precede, mirroring
        // cloud_probe.rs: the tracker emits at most one HB per interval.
        let mut out = Vec::with_capacity(frames.len());
        for mut frame in frames {
            if let Some(hb) = self.heartbeat.on_packet(&frame) {
                // Heartbeats are sentinels discarded before parse — no process.
                out.push(hb);
            }
            frame.process = process.clone();
            out.push(frame);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat::HEARTBEAT_INTERVAL_US;

    fn pump(allowlist: Vec<u32>) -> EbpfPump {
        // Offset 0: ktime µs maps straight to epoch µs, keeping test math simple.
        EbpfPump::new(SynthConfig::default(), BootClock::with_offset_us(0), allowlist)
    }

    fn tuple() -> ConnTuple {
        ConnTuple {
            client: "10.0.0.9:40000".parse().unwrap(),
            server: "203.0.113.7:443".parse().unwrap(),
        }
    }

    fn data_frame_count(pkts: &[RawPacket]) -> usize {
        pkts.iter().filter(|p| !p.is_heartbeat()).count()
    }

    #[test]
    fn boot_clock_converts_ktime_to_epoch() {
        let c = BootClock::with_offset_us(1_000_000);
        // 5_000_000 ns = 5_000 µs; + 1_000_000 µs offset = 1_005_000.
        assert_eq!(c.ktime_to_epoch_us(5_000_000), 1_005_000);
    }

    #[test]
    fn boot_clock_ns_to_us_divides_by_1000() {
        // With a zero offset the result is purely the ns→µs reduction. This is
        // the eBPF timestamp origin: a `bpf_ktime_get_ns` value (nanoseconds)
        // must become MICROSECONDS, never pass through as raw ns (which would be
        // 1000× too large and, read downstream as µs, land centuries ahead).
        let c = BootClock::with_offset_us(0);
        assert_eq!(c.ktime_to_epoch_us(1_000_000_000), 1_000_000); // 1 s
        assert_eq!(c.ktime_to_epoch_us(1_500), 1); // sub-µs truncates down
        assert_eq!(c.ktime_to_epoch_us(0), 0);
    }

    #[test]
    fn boot_clock_realistic_instant_is_microseconds_then_milliseconds() {
        // A realistic boot offset + monotonic ktime must yield an epoch value in
        // MICROSECONDS (~16 digits for 2026), and the µs→ms reduction the API
        // applies must then land within this century — the same invariant the
        // agent-turns API relies on. Ties the eBPF clock to the ms boundary.
        let offset_us = 1_781_000_000_000_000_i64; // boot≈2026 in µs
        let c = BootClock::with_offset_us(offset_us);
        let epoch_us = c.ktime_to_epoch_us(6_787_026_422); // ~6.8 s since boot
        // µs: 16-digit, recent.
        assert!((1_700_000_000_000_000..1_900_000_000_000_000).contains(&epoch_us));
        // µs→ms (what the API emits) stays below the year-2100 ceiling.
        assert!(epoch_us / 1_000 < 4_102_444_800_000);
    }

    #[test]
    fn boot_clock_large_ktime_does_not_overflow() {
        // ~292 years of uptime in ns still converts without i64 overflow.
        let c = BootClock::with_offset_us(0);
        let huge_ns = u64::MAX / 2;
        let us = c.ktime_to_epoch_us(huge_ns);
        assert_eq!(us, (huge_ns / 1_000) as i64);
    }

    #[test]
    fn connect_with_handshake_emits_syn_synack() {
        let mut p = pump(vec![]);
        let frames = p.on_event(SslEvent::Connect {
            conn_id: 1,
            pid: 100,
            comm: "python3".into(),
            exe: None,
            tuple: Some(tuple()),
            ktime_ns: 1_000_000,
        });
        assert_eq!(data_frame_count(&frames), 2, "SYN + SYN-ACK");
        assert_eq!(p.conn_count(), 1);
    }

    #[test]
    fn data_event_emits_segment() {
        let mut p = pump(vec![]);
        p.on_event(SslEvent::Connect {
            conn_id: 1,
            pid: 100,
            comm: "python3".into(),
            exe: None,
            tuple: Some(tuple()),
            ktime_ns: 1_000_000,
        });
        let frames = p.on_event(SslEvent::Data {
            conn_id: 1,
            pid: 100,
            comm: "python3".into(),
            exe: None,
            dir: StreamDir::ClientToServer,
            data: Bytes::from_static(b"POST /v1/messages HTTP/1.1\r\n\r\n"),
            seq_off: 0,
            ktime_ns: 1_100_000,
        });
        assert_eq!(data_frame_count(&frames), 1);
    }

    #[test]
    fn chunked_write_events_emit_sequential_segments() {
        // The BPF program splits a large SSL_write into several consecutive
        // DATA events on the same conn+dir. The pump must turn each into its own
        // TCP segment (one frame per event) so the TCP layer reassembles the
        // full body — never merging or dropping a chunk.
        let mut p = pump(vec![]);
        p.on_event(SslEvent::Connect {
            conn_id: 1,
            pid: 100,
            comm: "node".into(),
            exe: None,
            tuple: Some(tuple()),
            ktime_ns: 1_000_000,
        });
        let mut segments = 0;
        for (k, part) in [b"AAAA".as_slice(), b"BBBB", b"CCCC"].iter().enumerate() {
            let frames = p.on_event(SslEvent::Data {
                conn_id: 1,
                pid: 100,
                comm: "node".into(),
                exe: None,
                dir: StreamDir::ClientToServer,
                data: Bytes::copy_from_slice(part),
                // Each chunk carries its absolute stream offset (4 bytes each).
                seq_off: (k * 4) as u64,
                ktime_ns: 1_100_000 + k as u64 * 1000,
            });
            segments += data_frame_count(&frames);
        }
        assert_eq!(segments, 3, "each chunk event yields one TCP segment");
    }

    #[test]
    fn close_event_emits_fins_and_forgets_conn() {
        let mut p = pump(vec![]);
        p.on_event(SslEvent::Connect {
            conn_id: 1,
            pid: 100,
            comm: "c".into(),
            exe: None,
            tuple: Some(tuple()),
            ktime_ns: 1_000_000,
        });
        let frames = p.on_event(SslEvent::Close {
            conn_id: 1,
            ktime_ns: 1_200_000,
        });
        assert_eq!(data_frame_count(&frames), 2, "FIN both directions");
        assert_eq!(p.conn_count(), 0);
    }

    #[test]
    fn pid_allowlist_filters_other_processes() {
        let mut p = pump(vec![100]);
        // Allowed pid passes.
        let ok = p.on_event(SslEvent::Connect {
            conn_id: 1,
            pid: 100,
            comm: "c".into(),
            exe: None,
            tuple: Some(tuple()),
            ktime_ns: 1_000_000,
        });
        assert_eq!(data_frame_count(&ok), 2);
        // Disallowed pid is dropped — no frames, no connection registered.
        let dropped = p.on_event(SslEvent::Connect {
            conn_id: 2,
            pid: 999,
            comm: "other".into(),
            exe: None,
            tuple: Some(tuple()),
            ktime_ns: 1_050_000,
        });
        assert!(dropped.is_empty());
        assert_eq!(p.conn_count(), 1, "only the allowed connection registered");
    }

    #[test]
    fn unknown_tuple_falls_back_to_synthetic() {
        let mut p = pump(vec![]);
        // Connect with no recovered tuple → pump must synthesize one and still
        // emit the handshake.
        let frames = p.on_event(SslEvent::Connect {
            conn_id: 77,
            pid: 100,
            comm: "c".into(),
            exe: None,
            tuple: None,
            ktime_ns: 1_000_000,
        });
        assert_eq!(data_frame_count(&frames), 2);
        assert!(p.conn_count() == 1);
    }

    #[test]
    fn heartbeat_synthesized_after_interval() {
        let mut p = pump(vec![]);
        // First data primes the heartbeat baseline (no HB yet).
        let f0 = p.on_event(SslEvent::Data {
            conn_id: 1,
            pid: 100,
            comm: "c".into(),
            exe: None,
            dir: StreamDir::ClientToServer,
            data: Bytes::from_static(b"GET / HTTP/1.1\r\n"),
            seq_off: 0,
            ktime_ns: 1_000 * 1_000, // 1_000_000 ns = 1_000 µs
        });
        assert_eq!(f0.iter().filter(|p| p.is_heartbeat()).count(), 0);

        // A data event > 1 s later (in event-time µs) must be preceded by a
        // synthesized heartbeat.
        let later_ns = (1_000 + HEARTBEAT_INTERVAL_US as u64) * 1_000;
        let f1 = p.on_event(SslEvent::Data {
            conn_id: 1,
            pid: 100,
            comm: "c".into(),
            exe: None,
            dir: StreamDir::ClientToServer,
            data: Bytes::from_static(b"X"),
            seq_off: 16,
            ktime_ns: later_ns,
        });
        assert_eq!(
            f1.iter().filter(|p| p.is_heartbeat()).count(),
            1,
            "one heartbeat precedes the late frame"
        );
    }

    #[test]
    fn data_frames_carry_process_attribution() {
        let mut p = pump(vec![]);
        p.on_event(SslEvent::Connect {
            conn_id: 1,
            pid: 4242,
            comm: "python3".into(),
            exe: Some("/usr/bin/python3.12".into()),
            tuple: Some(tuple()),
            ktime_ns: 1_000_000,
        });
        let frames = p.on_event(SslEvent::Data {
            conn_id: 1,
            pid: 4242,
            comm: "python3".into(),
            exe: Some("/usr/bin/python3.12".into()),
            dir: StreamDir::ClientToServer,
            data: Bytes::from_static(b"POST /v1/messages HTTP/1.1\r\n\r\n"),
            seq_off: 0,
            ktime_ns: 1_100_000,
        });
        // The data segment carries the owning process; pid/comm/exe survive
        // synthesis intact.
        let data = frames
            .iter()
            .find(|f| !f.is_heartbeat())
            .expect("one data frame");
        let proc = data.process.as_ref().expect("process stamped");
        assert_eq!(proc.pid, 4242);
        assert_eq!(proc.comm, "python3");
        assert_eq!(proc.exe.as_deref(), Some("/usr/bin/python3.12"));
    }

    #[test]
    fn close_frames_carry_no_process() {
        let mut p = pump(vec![]);
        p.on_event(SslEvent::Connect {
            conn_id: 1,
            pid: 7,
            comm: "node".into(),
            exe: None,
            tuple: Some(tuple()),
            ktime_ns: 1_000_000,
        });
        let fins = p.on_event(SslEvent::Close {
            conn_id: 1,
            ktime_ns: 1_200_000,
        });
        // FIN frames carry no payload, hence no attribution to stamp.
        for f in fins.iter().filter(|f| !f.is_heartbeat()) {
            assert!(f.process.is_none());
        }
    }
}
