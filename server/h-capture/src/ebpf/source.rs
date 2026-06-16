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

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::unix::AsyncFd;
use tokio_util::sync::CancellationToken;

use aya::maps::RingBuf;
use aya::programs::UProbe;
use aya::Ebpf;

use h_common::config::{CaptureSourceConfig, EbpfTarget};
use h_common::internal_metrics::{Metric, MetricsWorker};
use h_ebpf_common::{kind, SslEvent as RawSslEvent, DATA_CAP};

use crate::ebpf::redact::Redactor;
use crate::ebpf::sigscan::{scan_elf_executable, Signature};
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
    targets: Vec<EbpfTarget>,
    pid_allowlist: Vec<u32>,
    segment_size: u32,
    /// Edge redactor built from config; `None` unless `redaction.enabled`.
    /// Applied to each plaintext buffer in the pump before frame synthesis.
    redactor: Option<Redactor>,
}

impl EbpfSource {
    pub fn from_config(
        config: &CaptureSourceConfig,
        _pcap_dump: Option<PacketDumperConfig>,
    ) -> crate::Result<Self> {
        let CaptureSourceConfig::Ebpf {
            source_id,
            ssl_libs,
            targets,
            pid_allowlist,
            segment_size,
            redaction,
        } = config
        else {
            return Err(crate::CaptureError::Other(
                "build_ebpf_source called with a non-ebpf config".to_string(),
            ));
        };
        let redactor = if redaction.enabled {
            Some(Redactor::new(
                redaction.headers.clone(),
                redaction.token_prefixes.clone(),
            ))
        } else {
            None
        };
        Ok(Self {
            source_id: source_id.clone().unwrap_or_else(|| "ebpf".to_string()),
            ssl_libs: ssl_libs.clone(),
            targets: targets.clone(),
            pid_allowlist: pid_allowlist.clone(),
            segment_size: *segment_size,
            redactor,
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

        let libs: Vec<PathBuf> = if self.ssl_libs.is_empty() {
            detect_libssl()
        } else {
            self.ssl_libs.iter().map(PathBuf::from).collect()
        }
        .into_iter()
        // Drop libs that don't exist on disk so an explicit-but-stale
        // `ssl_libs` entry doesn't fail the whole attach; also lets a
        // targets-only deployment pass a sentinel path to skip symbol attach.
        .filter(|p| p.exists())
        .collect();
        if libs.is_empty() && self.targets.is_empty() {
            return Err(crate::CaptureError::Other(
                "no libssl found and no static targets configured (set sources.ssl_libs or \
                 sources.targets)"
                    .to_string(),
            ));
        }

        // Load every BPF program once; a program is attached to many sites
        // (each libssl, each static target) but the kernel only accepts a
        // single `load()` per program.
        for name in [
            "ssl_write",
            "ssl_read_enter",
            "ssl_read_exit",
            "ssl_shutdown",
            "ssl_free",
        ] {
            load_program(&mut ebpf, name)?;
        }

        // Dynamically-linked OpenSSL/BoringSSL: attach by exported symbol.
        if !libs.is_empty() {
            tracing::info!("ebpf: attaching SSL uprobes to {:?}", libs);
            attach_sym(&mut ebpf, "ssl_write", "SSL_write", &libs, false)?;
            attach_sym(&mut ebpf, "ssl_read_enter", "SSL_read", &libs, false)?;
            attach_sym(&mut ebpf, "ssl_read_exit", "SSL_read", &libs, true)?;
            attach_sym(&mut ebpf, "ssl_shutdown", "SSL_shutdown", &libs, false)?;
            // SSL_free is the reliable teardown for dynamic libssl (many clients
            // never call SSL_shutdown). Best-effort: a libssl missing the symbol
            // shouldn't fail the whole attach, so log-and-continue, unlike the
            // load-bearing read/write probes above.
            if let Err(e) = attach_sym(&mut ebpf, "ssl_free", "SSL_free", &libs, false) {
                tracing::warn!("ebpf: SSL_free attach skipped: {e}");
            }
        }

        // Static, symbol-stripped targets (Phase 3, e.g. Claude Code's Bun
        // binary): locate SSL_read/SSL_write by byte signature and attach by
        // file offset. A target that yields no usable signature is logged and
        // skipped — it must not take down capture for the dynamic libs.
        //
        // Attach is **per inode**, not per on-disk path: npm-style auto-updates
        // (Claude Code, opencode) install a new build into a `.<pkg>-<hash>/`
        // staging dir and atomically rename it over the install path, so the
        // running session keeps an unlinked ("(deleted)") inode while a *new*
        // inode sits on disk. A path-only attach binds to whichever inode was on
        // disk at startup and silently misses both the already-running sessions
        // (deleted inode) and every post-update session (new inode). We instead
        // enumerate the on-disk path *and* every running target process via
        // `/proc/<pid>/exe` (which the kernel resolves to the real inode even
        // when deleted — verified accepting uprobes on this kernel), attach once
        // per distinct inode, and re-scan periodically to catch new inodes from
        // updates and freshly-spawned sessions.
        for target in &self.targets {
            if !target_has_source(target) {
                tracing::warn!(
                    "ebpf: target {} (flavor={}) has no offset or signature — set \
                     write_offset/read_offset or write_sig/read_sig in config (or use a \
                     flavor with built-in signatures, e.g. \"bun\")",
                    target.binary,
                    target.flavor
                );
            }
        }
        let libs_count = libs.len();
        // `seen` = every inode we've attempted (success or deterministic sig
        // failure) so we neither re-attach nor re-warn on each rescan; `attached`
        // = inodes actually carrying probes (drives the health gauge).
        let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
        let mut attached_inodes: HashSet<(u64, u64)> = HashSet::new();
        rescan_targets(
            &mut ebpf,
            &self.targets,
            &mut seen_inodes,
            &mut attached_inodes,
        );
        if libs.is_empty() && attached_inodes.is_empty() {
            return Err(crate::CaptureError::Other(
                "no uprobes attached: every configured static target failed signature \
                 resolution (check `flavor` / `write_sig` / `read_sig`)"
                    .to_string(),
            ));
        }
        // Attach-health gauge: count of binaries carrying SSL uprobes (dynamic
        // libssl + resolved static-target inodes). 0 would have errored above, so
        // a healthy source reports ≥1.
        metrics
            .counter(Metric::EbpfUprobesAttached)
            .set((libs_count + attached_inodes.len()) as u64);

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
        let mut pump = EbpfPump::new(cfg, BootClock::from_system(), self.pid_allowlist.clone())
            .with_redactor(self.redactor.clone());
        if self.redactor.is_some() {
            tracing::info!("ebpf: edge redaction enabled");
        }

        let mut idle_hb = tokio::time::interval(Duration::from_secs(1));
        idle_hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Periodically re-scan target processes so probes follow npm-style
        // inode rotation (auto-update) and reach newly-spawned sessions without
        // a service restart. The first tick fires immediately but is a no-op:
        // every inode is already in `seen` from the startup rescan above. Only
        // meaningful when `targets` is non-empty; harmless otherwise.
        let mut rescan = tokio::time::interval(Duration::from_secs(RESCAN_INTERVAL_SECS));
        rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                _ = rescan.tick() => {
                    let newly = rescan_targets(
                        &mut ebpf,
                        &self.targets,
                        &mut seen_inodes,
                        &mut attached_inodes,
                    );
                    if newly > 0 {
                        tracing::info!(
                            "ebpf: rescan attached {newly} new target inode(s) \
                             (total target inodes={})",
                            attached_inodes.len()
                        );
                        metrics
                            .counter(Metric::EbpfUprobesAttached)
                            .set((libs_count + attached_inodes.len()) as u64);
                    }
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
                        let Some(ev) = decode_event(&item, &mut exe_cache) else {
                            metrics.counter(Metric::EbpfEventsDropped).inc();
                            continue;
                        };
                        metrics.counter(Metric::EbpfEventsReceived).inc();
                        if let SslEvent::Data { ref data, .. } = ev {
                            metrics.counter(Metric::EbpfBytesCaptured).add(data.len() as u64);
                        }
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
                                metrics.counter(Metric::EbpfFramesSynthesized).inc();
                            }
                        }
                    }
                    guard.clear_ready();
                    // Refresh live gauges after draining the ring batch.
                    metrics
                        .counter(Metric::EbpfConnectionsActive)
                        .set(pump.conn_count() as u64);
                    metrics
                        .counter(Metric::EbpfProcessCacheSize)
                        .set(exe_cache.len() as u64);
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
                seq_off: raw.seq_off,
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

/// Load a BPF program by name. Called once per program before any attach,
/// because the kernel rejects a second `load()` while the program is attached
/// at multiple sites (each libssl + each static target).
fn load_program(ebpf: &mut Ebpf, prog_name: &str) -> crate::Result<()> {
    let program: &mut UProbe = ebpf
        .program_mut(prog_name)
        .ok_or_else(|| crate::CaptureError::Other(format!("program {prog_name} missing")))?
        .try_into()
        .map_err(|e| crate::CaptureError::Other(format!("program {prog_name} not a uprobe: {e}")))?;
    program
        .load()
        .map_err(|e| crate::CaptureError::Other(format!("load {prog_name}: {e}")))?;
    Ok(())
}

/// Attach an already-loaded program to `symbol` in each dynamically-linked
/// library (uretprobe vs uprobe is fixed by the program definition; `ret` is
/// only for diagnostics).
fn attach_sym(
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

/// Attach an already-loaded program to a static binary at a resolved **file
/// offset** (no symbol). The path for the Bun/BoringSSL Phase-3 case.
fn attach_offset(
    ebpf: &mut Ebpf,
    prog_name: &str,
    offset: u64,
    binary: &std::path::Path,
    ret: bool,
) -> crate::Result<()> {
    let program: &mut UProbe = ebpf
        .program_mut(prog_name)
        .ok_or_else(|| crate::CaptureError::Other(format!("program {prog_name} missing")))?
        .try_into()
        .map_err(|e| crate::CaptureError::Other(format!("program {prog_name} not a uprobe: {e}")))?;
    program
        .attach(None, offset, binary, None)
        .map_err(|e| {
            crate::CaptureError::Other(format!(
                "attach {prog_name} at offset {offset:#x} in {} (ret={ret}): {e}",
                binary.display()
            ))
        })?;
    Ok(())
}

/// Built-in BoringSSL prologue signatures for a flavor, using the anchor + window
/// technique. `SSL_read`'s prologue is distinctive enough to match uniquely; the
/// `SSL_write` prologue is generic (a common register-save sequence appears many
/// times), so it is located as the nearest match in a window *after* the
/// `SSL_read` anchor — robust to the small per-build drift in the inter-function
/// distance that a hardcoded delta would miss.
struct FlavorSig {
    /// Distinctive `SSL_read` prologue — must match uniquely (the anchor).
    read_sig: &'static str,
    /// `SSL_write` prologue — generic; resolved as the first match within
    /// `write_window` bytes after the `SSL_read` anchor.
    write_sig: &'static str,
    write_window: u64,
}

/// Built-in signatures per flavor. Returns `None` for `boringssl` (generic): a
/// prologue is specific to one statically-linked build, so a bare `boringssl`
/// target must supply `write_sig`/`read_sig`/`*_offset` in config.
///
/// The `bun` signatures are the BoringSSL `SSL_read`/`SSL_write` x86-64
/// prologues from Bun v1.3.x profile builds (the runtime Claude Code ships),
/// matching the read-anchored, windowed-write approach from the eunomia-bpf
/// AgentSight project (MIT). They are still version-bound data — a future Bun
/// line may shift the prologue; override via config when that happens.
fn flavor_signatures(flavor: &str) -> Option<FlavorSig> {
    match flavor {
        "bun" | "boringssl-bun" | "claude-code" => Some(FlavorSig {
            read_sig: "55 48 89 e5 41 57 41 56 53 50 48 83 bf 98 00 00 00 00 74",
            write_sig:
                "55 48 89 e5 41 57 41 56 41 55 41 54 53 48 83 ec 18 41 89 d7 49 89 f6 48 89 fb",
            write_window: 0x10000,
        }),
        _ => None,
    }
}

/// Locate a function as the first signature match within `window` bytes after an
/// `anchor` offset. Used for the generic `SSL_write` prologue once the unique
/// `SSL_read` anchor is known — handles a prologue that occurs many times across
/// the binary by scoping to the SSL function's neighborhood.
fn resolve_windowed(
    data: &[u8],
    pattern: &str,
    anchor: u64,
    window: u64,
    what: &str,
    binary: &str,
) -> Option<u64> {
    let sig = Signature::parse(pattern)?;
    let hit = scan_elf_executable(data, &sig)
        .into_iter()
        .find(|&o| o >= anchor && o < anchor.saturating_add(window));
    match hit {
        Some(off) => {
            tracing::info!("ebpf: {binary}: {what} resolved at offset {off:#x} (anchored)");
            Some(off)
        }
        None => {
            tracing::warn!(
                "ebpf: {binary}: {what} not found within {window:#x} of anchor {anchor:#x}"
            );
            None
        }
    }
}

/// Resolve a unique uprobe file offset for `pattern` in `data`. Requires
/// exactly one match: zero means a stale/wrong signature (skip, don't attach
/// blindly), and more than one is ambiguous (a too-loose signature would attach
/// the probe to the wrong function). Both cases log and return `None`.
fn resolve_single_offset(data: &[u8], pattern: &str, what: &str, binary: &str) -> Option<u64> {
    let Some(sig) = Signature::parse(pattern) else {
        tracing::warn!("ebpf: {binary}: malformed {what} signature {pattern:?}");
        return None;
    };
    let hits = scan_elf_executable(data, &sig);
    match hits.as_slice() {
        [] => {
            tracing::warn!("ebpf: {binary}: {what} signature matched nothing (wrong build?)");
            None
        }
        [off] => {
            tracing::info!("ebpf: {binary}: {what} resolved at offset {off:#x}");
            Some(*off)
        }
        many => {
            tracing::warn!(
                "ebpf: {binary}: {what} signature is ambiguous ({} matches) — refine it",
                many.len()
            );
            None
        }
    }
}

/// How often to re-scan target processes for new inodes (auto-update rotation)
/// and freshly-spawned sessions. Short enough that a new Claude Code / opencode
/// session is captured within seconds of starting; cheap (a `/proc` walk plus a
/// signature scan only for inodes not seen before).
const RESCAN_INTERVAL_SECS: u64 = 15;

/// Does this target carry enough config to resolve uprobe offsets at all?
/// (an explicit offset, a config signature, or a flavor with built-in sigs.)
fn target_has_source(target: &EbpfTarget) -> bool {
    target.write_offset.is_some()
        || target.read_offset.is_some()
        || target.write_sig.is_some()
        || target.read_sig.is_some()
        || flavor_signatures(&target.flavor).is_some()
}

/// True if a `/proc/<pid>/exe` readlink target has the given basename. The
/// kernel suffixes the link with `" (deleted)"` once the binary is unlinked by
/// an auto-update, so strip that first. Matching by **basename** (not full path)
/// is what lets us re-attach across npm's atomic-rename upgrade, which stages the
/// new build in a `.<pkg>-<hash>/` dir before renaming it over the install path —
/// the running process's exe then points into that now-deleted staging dir.
fn exe_link_has_basename(link: &str, basename: &str) -> bool {
    let path = link.strip_suffix(" (deleted)").unwrap_or(link);
    Path::new(path).file_name().and_then(|f| f.to_str()) == Some(basename)
}

/// (dev, inode) identity of `path`, following symlinks — so `/proc/<pid>/exe`
/// resolves to the real (possibly deleted) inode. Returns `None` if it can't be
/// stat'd (e.g. the process exited between enumeration and here).
fn inode_of(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// PIDs whose `/proc/<pid>/exe` resolves to a binary named `basename`.
fn target_pids(basename: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in rd.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(link) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            if exe_link_has_basename(&link.to_string_lossy(), basename) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Resolve SSL_read/SSL_write **file offsets** for a target from the given
/// binary `data` (config offsets first, then config signatures, then built-in
/// flavor signatures). Pure over `data` — same bytes always yield the same
/// offsets, so a per-inode result can be cached as "seen".
fn resolve_target_offsets(
    data: &[u8],
    target: &EbpfTarget,
    label: &str,
) -> (Option<u64>, Option<u64>) {
    let mut read_off = target.read_offset;
    let mut write_off = target.write_offset;
    if read_off.is_some() && write_off.is_some() {
        return (read_off, write_off);
    }

    // Config-supplied signatures take precedence and must match uniquely.
    if read_off.is_none() {
        if let Some(p) = &target.read_sig {
            read_off = resolve_single_offset(data, p, "SSL_read", label);
        }
    }
    if write_off.is_none() {
        if let Some(p) = &target.write_sig {
            write_off = resolve_single_offset(data, p, "SSL_write", label);
        }
    }

    // Fall back to built-in flavor signatures: anchor on the unique SSL_read
    // prologue, then locate SSL_write (generic prologue) as the nearest match in
    // the window after it.
    if let Some(fs) = flavor_signatures(&target.flavor) {
        if read_off.is_none() {
            read_off = resolve_single_offset(data, fs.read_sig, "SSL_read", label);
        }
        if write_off.is_none() {
            match read_off {
                Some(anchor) => {
                    write_off = resolve_windowed(
                        data,
                        fs.write_sig,
                        anchor,
                        fs.write_window,
                        "SSL_write",
                        label,
                    );
                }
                None => tracing::warn!(
                    "ebpf: {label}: no SSL_read anchor — cannot locate SSL_write by window"
                ),
            }
        }
    }
    (read_off, write_off)
}

/// Attach the (already-loaded) SSL uprobes to `attach_path` at the resolved
/// offsets. `attach_path` may be the on-disk binary or a `/proc/<pid>/exe`
/// handle to a deleted inode — the kernel resolves both to the same inode and
/// installs the probe on it. Returns `Ok(true)` if at least one probe attached.
fn attach_at(
    ebpf: &mut Ebpf,
    attach_path: &Path,
    read_off: Option<u64>,
    write_off: Option<u64>,
) -> crate::Result<bool> {
    let mut any = false;
    if let Some(off) = write_off {
        attach_offset(ebpf, "ssl_write", off, attach_path, false)?;
        any = true;
    }
    if let Some(off) = read_off {
        // Entry probe captures the buffer pointer; the return probe reads the
        // bytes SSL_read filled in. Both attach at the function entry.
        attach_offset(ebpf, "ssl_read_enter", off, attach_path, false)?;
        attach_offset(ebpf, "ssl_read_exit", off, attach_path, true)?;
        any = true;
    }
    Ok(any)
}

/// Attach SSL uprobes to every distinct inode backing the configured static
/// targets, covering both the on-disk install path (future fresh execs of the
/// current build) and every running session via `/proc/<pid>/exe` (already-
/// running sessions on a deleted inode after an auto-update). Idempotent across
/// calls: `seen` skips any inode already attempted, `attached` records inodes
/// actually carrying probes. Returns the count of inodes newly attached this
/// pass. Never propagates errors — a bad target must not take down capture.
fn rescan_targets(
    ebpf: &mut Ebpf,
    targets: &[EbpfTarget],
    seen: &mut HashSet<(u64, u64)>,
    attached: &mut HashSet<(u64, u64)>,
) -> usize {
    let mut newly = 0;
    for target in targets {
        if !target_has_source(target) {
            continue; // warned once at startup
        }
        let basename = Path::new(&target.binary)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(target.binary.as_str());

        // On-disk path first (a stable handle, preferred when it backs the same
        // inode a running session uses), then each running session's exe.
        let mut candidates: Vec<PathBuf> = Vec::new();
        let on_disk = PathBuf::from(&target.binary);
        if on_disk.exists() {
            candidates.push(on_disk);
        }
        candidates.extend(
            target_pids(basename)
                .into_iter()
                .map(|pid| PathBuf::from(format!("/proc/{pid}/exe"))),
        );

        for path in candidates {
            let Some(ino) = inode_of(&path) else {
                continue; // process exited mid-scan; retry next pass
            };
            if seen.contains(&ino) {
                continue;
            }
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                // Transient (process exited) — don't mark seen, retry next pass.
                Err(e) => {
                    tracing::debug!("ebpf: rescan: read {} failed: {e}", path.display());
                    continue;
                }
            };
            // Deterministic over these bytes: record seen now so a sig miss
            // doesn't re-warn every interval.
            seen.insert(ino);
            let (read_off, write_off) = resolve_target_offsets(&data, target, &target.binary);
            match attach_at(ebpf, &path, read_off, write_off) {
                Ok(true) => {
                    attached.insert(ino);
                    newly += 1;
                    tracing::info!(
                        "ebpf: attached {} via {} (inode {}:{})",
                        target.binary,
                        path.display(),
                        ino.0,
                        ino.1
                    );
                }
                Ok(false) => tracing::warn!(
                    "ebpf: {} via {}: no usable SSL offset (wrong signature/flavor?)",
                    target.binary,
                    path.display()
                ),
                Err(e) => tracing::warn!(
                    "ebpf: attach {} via {} failed: {e}",
                    target.binary,
                    path.display()
                ),
            }
        }
    }
    newly
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

#[cfg(test)]
mod tests {
    use super::*;

    fn target(binary: &str, flavor: &str) -> EbpfTarget {
        EbpfTarget {
            binary: binary.to_string(),
            flavor: flavor.to_string(),
            write_sig: None,
            read_sig: None,
            write_offset: None,
            read_offset: None,
        }
    }

    #[test]
    fn exe_link_basename_matches_plain_path() {
        assert!(exe_link_has_basename(
            "/home/user/.nvm/.../claude-code/bin/claude.exe",
            "claude.exe"
        ));
    }

    #[test]
    fn exe_link_basename_matches_through_deleted_suffix() {
        // After an npm atomic-rename auto-update the running process's exe points
        // into the now-unlinked staging dir; the kernel appends " (deleted)".
        // Basename matching must see through both the staging dir and the suffix.
        assert!(exe_link_has_basename(
            "/home/user/.nvm/.../@anthropic-ai/.claude-code-BLnYIOGh/bin/claude.exe (deleted)",
            "claude.exe"
        ));
        assert!(exe_link_has_basename(
            "/home/user/.nvm/.../opencode-ai/bin/opencode.exe (deleted)",
            "opencode.exe"
        ));
    }

    #[test]
    fn exe_link_basename_rejects_other_binaries() {
        assert!(!exe_link_has_basename("/usr/bin/node", "claude.exe"));
        assert!(!exe_link_has_basename(
            "/some/where/claude.exe.bak (deleted)",
            "claude.exe"
        ));
    }

    #[test]
    fn target_has_source_requires_offset_sig_or_flavor() {
        // Bare boringssl flavor with no offsets/sigs → not enough to attach.
        assert!(!target_has_source(&target("/x/claude.exe", "boringssl")));
        // A known flavor with built-in signatures is enough.
        assert!(target_has_source(&target("/x/claude.exe", "bun")));
        // An explicit offset is enough regardless of flavor.
        let mut t = target("/x/claude.exe", "boringssl");
        t.read_offset = Some(0x1000);
        assert!(target_has_source(&t));
    }

    #[test]
    fn resolve_offsets_passes_config_offsets_through_without_scanning() {
        let mut t = target("/x/claude.exe", "boringssl");
        t.read_offset = Some(0x4165_5e0);
        t.write_offset = Some(0x4165_970);
        // Empty data would make any scan fail; config offsets must short-circuit.
        let (r, w) = resolve_target_offsets(&[], &t, "test");
        assert_eq!(r, Some(0x4165_5e0));
        assert_eq!(w, Some(0x4165_970));
    }
}
