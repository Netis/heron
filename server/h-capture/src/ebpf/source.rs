//! Linux aya loader for eBPF SSL-uprobe capture.
//!
//! Loads the embedded BPF program (`h-ebpf-prog`, built by `build.rs`), attaches
//! uprobes to `SSL_write` / `SSL_read` / `SSL_shutdown` on the host's `libssl`,
//! polls the ring buffer, decodes [`RawSslEvent`] records into the
//! cross-platform [`SslEvent`], and drives an [`EbpfPump`] whose synthesized
//! [`RawPacket`]s flow into the standard pipeline.
//!
//! Phase 1 MVP: no connect-side 5-tuple recovery yet — connections use a
//! synthetic tuple and the reassembler syncs mid-stream on the first request
//! line (`emit_handshake = false`). Real-tuple recovery via a `tcp_connect`
//! kprobe is a follow-up refinement.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::unix::AsyncFd;
use tokio_util::sync::CancellationToken;

use aya::maps::RingBuf;
use aya::programs::UProbe;
use aya::Ebpf;

use h_common::config::CaptureSourceConfig;
use h_common::internal_metrics::{Metric, MetricsWorker};
use h_ebpf_common::{kind, SslEvent as RawSslEvent, DATA_CAP};

use crate::ebpf::{BootClock, EbpfPump, SslEvent};
use crate::packet::RawPacket;
use crate::pcap_dump::PacketDumperConfig;
use crate::routing::RoutingSender;
use crate::source::CaptureSource;
use crate::synth::{StreamDir, SynthConfig};

/// eBPF SSL-uprobe capture source.
pub struct EbpfSource {
    source_id: String,
    ssl_libs: Vec<String>,
    pid_allowlist: Vec<u32>,
    segment_size: u32,
}

impl EbpfSource {
    pub fn from_config(
        config: &CaptureSourceConfig,
        _pcap_dump: Option<PacketDumperConfig>,
    ) -> crate::Result<Self> {
        let CaptureSourceConfig::Ebpf {
            source_id,
            ssl_libs,
            pid_allowlist,
            segment_size,
            ..
        } = config
        else {
            return Err(crate::CaptureError::Other(
                "build_ebpf_source called with a non-ebpf config".to_string(),
            ));
        };
        Ok(Self {
            source_id: source_id.clone().unwrap_or_else(|| "ebpf".to_string()),
            ssl_libs: ssl_libs.clone(),
            pid_allowlist: pid_allowlist.clone(),
            segment_size: *segment_size,
        })
    }
}

#[async_trait]
impl CaptureSource for EbpfSource {
    async fn run(
        self: Box<Self>,
        tx: RoutingSender,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        // Loosen the kernel lock limit so the BPF program + maps can be loaded.
        if let Err(e) = bump_memlock_rlimit() {
            tracing::warn!("ebpf: could not raise RLIMIT_MEMLOCK: {e}");
        }

        let mut ebpf = Ebpf::load(aya::include_bytes_aligned!(concat!(
            env!("OUT_DIR"),
            "/h-ebpf-prog"
        )))
        .map_err(|e| crate::CaptureError::Other(format!("load BPF program: {e}")))?;

        let libs = if self.ssl_libs.is_empty() {
            detect_libssl()
        } else {
            self.ssl_libs.iter().map(PathBuf::from).collect()
        };
        if libs.is_empty() {
            return Err(crate::CaptureError::Other(
                "no libssl found to attach to (set sources.ssl_libs explicitly)".to_string(),
            ));
        }
        tracing::info!("ebpf: attaching SSL uprobes to {:?}", libs);

        attach_uprobe(&mut ebpf, "ssl_write", "SSL_write", &libs, false)?;
        attach_uprobe(&mut ebpf, "ssl_read_enter", "SSL_read", &libs, false)?;
        attach_uprobe(&mut ebpf, "ssl_read_exit", "SSL_read", &libs, true)?;
        attach_uprobe(&mut ebpf, "ssl_shutdown", "SSL_shutdown", &libs, false)?;

        let ring = RingBuf::try_from(
            ebpf.take_map("EVENTS")
                .ok_or_else(|| crate::CaptureError::Other("EVENTS map missing".to_string()))?,
        )
        .map_err(|e| crate::CaptureError::Other(format!("ring buffer: {e}")))?;
        let mut async_fd = AsyncFd::new(ring)
            .map_err(|e| crate::CaptureError::Other(format!("async ring fd: {e}")))?;

        let cfg = SynthConfig {
            source_id: self.source_id.clone(),
            segment_size: self.segment_size as usize,
            // No connect event / real tuple yet → rely on mid-stream sync.
            emit_handshake: false,
        };
        let mut pump = EbpfPump::new(cfg, BootClock::from_system(), self.pid_allowlist.clone());

        let mut idle_hb = tokio::time::interval(Duration::from_secs(1));
        idle_hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!("ebpf: capture started (source_id={})", self.source_id);

        // pid → resolved `/proc/<pid>/exe`, memoized so we readlink once per
        // process rather than once per event. Bounded: cleared if it grows past
        // a generous ceiling (defends against pid churn over a long capture).
        let mut exe_cache: HashMap<u32, Option<String>> = HashMap::new();

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::debug!("ebpf: cancellation requested, stopping");
                    break;
                }
                _ = idle_hb.tick() => {
                    // Drive downstream time-advance during traffic idle.
                    let hb = RawPacket::heartbeat(now_epoch_us(), self.source_id.clone());
                    if tx.send(hb).await.is_err() {
                        break;
                    }
                    metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                }
                guard = async_fd.readable_mut() => {
                    let mut guard = match guard {
                        Ok(g) => g,
                        Err(e) => {
                            tracing::warn!("ebpf: ring readable error: {e}");
                            continue;
                        }
                    };
                    let ring = guard.get_inner_mut();
                    while let Some(item) = ring.next() {
                        let Some(ev) = decode_event(&item, &mut exe_cache) else { continue };
                        for pkt in pump.on_event(ev) {
                            let is_hb = pkt.is_heartbeat();
                            if tx.send(pkt).await.is_err() {
                                tracing::debug!("ebpf: channel closed, stopping");
                                return Ok(());
                            }
                            if is_hb {
                                metrics.counter(Metric::CaptureHeartbeatsEmitted).inc();
                            } else {
                                metrics.counter(Metric::CapturePacketsReceived).inc();
                            }
                        }
                    }
                    guard.clear_ready();
                }
            }
        }
        Ok(())
    }
}

/// Cap on the pid→exe memo so a long capture across heavy pid churn can't grow
/// it without bound. Cleared wholesale on overflow (cheap; re-warms on demand).
const EXE_CACHE_CAP: usize = 4096;

/// Decode one ring-buffer record into a cross-platform [`SslEvent`].
fn decode_event(bytes: &[u8], exe_cache: &mut HashMap<u32, Option<String>>) -> Option<SslEvent> {
    if bytes.len() < RawSslEvent::SIZE {
        return None;
    }
    // The ring buffer gives an unaligned slice; read the POD struct unaligned.
    let raw: RawSslEvent = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const RawSslEvent) };
    let comm = comm_to_string(&raw.comm);
    match raw.kind {
        kind::CLOSE => Some(SslEvent::Close {
            conn_id: raw.conn_id,
            ktime_ns: raw.ktime_ns,
        }),
        kind::DATA_WRITE | kind::DATA_READ => {
            let len = (raw.data_len as usize).min(DATA_CAP);
            let dir = if raw.kind == kind::DATA_WRITE {
                StreamDir::ClientToServer
            } else {
                StreamDir::ServerToClient
            };
            Some(SslEvent::Data {
                conn_id: raw.conn_id,
                pid: raw.pid,
                comm,
                exe: resolve_exe(raw.pid, exe_cache),
                dir,
                data: Bytes::copy_from_slice(&raw.data[..len]),
                ktime_ns: raw.ktime_ns,
            })
        }
        _ => None,
    }
}

/// Best-effort `/proc/<pid>/exe` resolution, memoized. Returns the absolute
/// executable path, or `None` when the link can't be read (process exited,
/// permission denied). Requires the capturing process to out-rank the target —
/// satisfied when running as root / CAP_SYS_PTRACE, which the eBPF source needs
/// anyway.
fn resolve_exe(pid: u32, cache: &mut HashMap<u32, Option<String>>) -> Option<String> {
    if let Some(v) = cache.get(&pid) {
        return v.clone();
    }
    if cache.len() >= EXE_CACHE_CAP {
        cache.clear();
    }
    let resolved = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    cache.insert(pid, resolved.clone());
    resolved
}

fn comm_to_string(comm: &[u8]) -> String {
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    String::from_utf8_lossy(&comm[..end]).into_owned()
}

/// Attach one uprobe (or uretprobe when `ret`) to `symbol` in each library.
fn attach_uprobe(
    ebpf: &mut Ebpf,
    prog_name: &str,
    symbol: &str,
    libs: &[PathBuf],
    ret: bool,
) -> crate::Result<()> {
    let program: &mut UProbe = ebpf
        .program_mut(prog_name)
        .ok_or_else(|| crate::CaptureError::Other(format!("program {prog_name} missing")))?
        .try_into()
        .map_err(|e| crate::CaptureError::Other(format!("program {prog_name} not a uprobe: {e}")))?;
    program
        .load()
        .map_err(|e| crate::CaptureError::Other(format!("load {prog_name}: {e}")))?;
    let mut attached = 0;
    for lib in libs {
        match program.attach(Some(symbol), 0, lib, None) {
            Ok(_) => attached += 1,
            Err(e) => tracing::warn!(
                "ebpf: attach {prog_name} to {symbol} in {} failed: {e}",
                lib.display()
            ),
        }
    }
    if attached == 0 {
        return Err(crate::CaptureError::Other(format!(
            "could not attach {prog_name} to {symbol} on any libssl (ret={ret})"
        )));
    }
    Ok(())
}

/// Discover `libssl` shared objects on the host. Tries `ldconfig -p` first, then
/// falls back to well-known multiarch paths.
fn detect_libssl() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(out) = std::process::Command::new("ldconfig").arg("-p").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.contains("libssl.so") {
                if let Some(path) = line.split("=>").nth(1) {
                    let p = PathBuf::from(path.trim());
                    if p.exists() && !found.contains(&p) {
                        found.push(p);
                    }
                }
            }
        }
    }
    if found.is_empty() {
        for cand in [
            "/usr/lib/x86_64-linux-gnu/libssl.so.3",
            "/usr/lib/x86_64-linux-gnu/libssl.so.1.1",
            "/lib/x86_64-linux-gnu/libssl.so.3",
            "/usr/lib/aarch64-linux-gnu/libssl.so.3",
        ] {
            let p = PathBuf::from(cand);
            if p.exists() {
                found.push(p);
            }
        }
    }
    found
}

fn now_epoch_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Raise `RLIMIT_MEMLOCK` to infinity so map/program allocation isn't capped on
/// older kernels. On kernels with the BPF memcg accounting this is a no-op.
fn bump_memlock_rlimit() -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &limit) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
