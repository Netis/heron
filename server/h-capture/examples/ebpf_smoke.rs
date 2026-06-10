//! Live eBPF capture smoke test.
//!
//! Runs the eBPF `SSL_read`/`SSL_write` capture source and prints the HTTP
//! request lines it reconstructs from synthesized frames, each tagged with the
//! owning process (`comm`/pid/exe) the kernel attributed it to. Proves the
//! uprobe → ring buffer → frame-synthesis → process-attribution path against
//! real `libssl` traffic without the rest of the pipeline.
//!
//! Usage (Linux, needs CAP_BPF / root):
//!   sudo -E ~/.cargo/bin/cargo run -p h-capture --features ebpf --example ebpf_smoke
//! then, in another shell, generate TLS traffic:
//!   python3 -c "import urllib.request as u; u.urlopen('https://example.com').read()"
//!
//! Exits after ~20s or once it has printed a few requests.

use std::time::Duration;

use h_capture::{build_source, RawPacket};
use h_common::config::CaptureSourceConfig;
use h_common::internal_metrics::{Metric, MetricsSystem};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    // Phase-3 mode: when HERON_EBPF_TARGET_BIN is set, attach by byte-signature
    // offset to a static target instead of dynamic libssl. `*_SIG` are prologue
    // patterns (see `sigscan_probe`). A sentinel `ssl_libs` path is filtered out
    // by the loader, isolating the offset-attach path so this exercises Phase 3
    // end-to-end (signature → scan → offset → attach → capture).
    let (ssl_libs, targets) = match std::env::var("HERON_EBPF_TARGET_BIN") {
        Ok(bin) if !bin.is_empty() => {
            let parse_off = |k: &str| -> Option<u64> {
                std::env::var(k).ok().and_then(|s| {
                    let s = s.trim();
                    let s = s.strip_prefix("0x").unwrap_or(s);
                    u64::from_str_radix(s, 16).ok()
                })
            };
            let target = h_common::config::EbpfTarget {
                binary: bin,
                flavor: std::env::var("HERON_EBPF_FLAVOR").unwrap_or_else(|_| "boringssl".into()),
                write_sig: std::env::var("HERON_EBPF_WRITE_SIG").ok().filter(|s| !s.is_empty()),
                read_sig: std::env::var("HERON_EBPF_READ_SIG").ok().filter(|s| !s.is_empty()),
                write_offset: parse_off("HERON_EBPF_WRITE_OFFSET"),
                read_offset: parse_off("HERON_EBPF_READ_OFFSET"),
            };
            eprintln!("ebpf-smoke: Phase-3 offset-attach target = {}", target.binary);
            (vec!["/heron/no-dynamic-libssl".to_string()], vec![target])
        }
        _ => (vec![], vec![]), // default: autodetect dynamic libssl
    };

    let config = CaptureSourceConfig::Ebpf {
        source_id: Some("ebpf-smoke".to_string()),
        ssl_libs,
        targets,
        pid_allowlist: vec![], // all processes
        segment_size: 16 * 1024,
    };

    let source = build_source(&config, None).expect("build ebpf source");

    let mut sys = MetricsSystem::new();
    let metrics = sys.register_worker("ebpf-smoke", Metric::ALL);
    let _ = sys.start();

    let (tx, mut rx) = mpsc::channel::<RawPacket>(1024);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = source
            .run(h_capture::RoutingSender::single(tx), metrics, cancel2)
            .await
        {
            eprintln!("source error: {e}");
        }
    });

    println!("ebpf-smoke: capturing for 20s — generate HTTPS traffic now...");
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);

    let mut pkts = 0u64;
    let mut req_lines = 0u64;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            maybe = rx.recv() => {
                let Some(pkt) = maybe else { break };
                if pkt.is_heartbeat() {
                    continue;
                }
                pkts += 1;
                // The differentiating value of the eBPF path: each frame is
                // attributed to the local process that owns the connection.
                let who = pkt
                    .process
                    .as_ref()
                    .map(|p| {
                        let exe = p.exe.as_deref().map(|e| format!(" {e}")).unwrap_or_default();
                        format!("{}({}){}", p.comm, p.pid, exe)
                    })
                    .unwrap_or_else(|| "—".to_string());
                if let Some(line) = http_first_line(&pkt.data) {
                    req_lines += 1;
                    println!("  [{pkts:>4}] {who:<40} {line}");
                    if req_lines >= 8 {
                        break;
                    }
                } else if pkts <= 12 {
                    // Even when the payload isn't an HTTP/1.x first line (HTTP/2
                    // framing, TLS control, mid-body), show the attribution +
                    // a printable preview so the process tagging is visible live.
                    println!("  [{pkts:>4}] {who:<40} {}", payload_preview(&pkt.data));
                }
            }
        }
    }

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    println!("ebpf-smoke: done — {pkts} data frames, {req_lines} HTTP request/response lines");
    if req_lines == 0 {
        eprintln!("ebpf-smoke: no HTTP lines captured — check libssl detection / privileges");
        std::process::exit(1);
    }
}

/// Return the TCP payload of a synthesized IPv4 frame, if any.
fn tcp_payload(frame: &[u8]) -> Option<&[u8]> {
    const ETH: usize = 14;
    if frame.len() < ETH + 20 + 20 || (frame[ETH] >> 4) != 4 {
        return None;
    }
    let ihl = ((frame[ETH] & 0x0f) as usize) * 4;
    let tcp = ETH + ihl;
    if frame.len() < tcp + 20 {
        return None;
    }
    let data_off = ((frame[tcp + 12] >> 4) as usize) * 4;
    frame.get(tcp + data_off..)
}

/// A short printable preview of a frame's TCP payload (non-printable bytes as
/// `.`), for frames that aren't an HTTP/1.x first line.
fn payload_preview(frame: &[u8]) -> String {
    let Some(p) = tcp_payload(frame) else {
        return "(no payload)".to_string();
    };
    if p.is_empty() {
        return "(empty / control)".to_string();
    }
    let preview: String = p
        .iter()
        .take(40)
        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' })
        .collect();
    format!("{}B: {preview}", p.len())
}

/// Extract the first line of an HTTP message from a synthesized Ethernet+IPv4
/// +TCP frame, if the TCP payload starts with an HTTP request method or status.
fn http_first_line(frame: &[u8]) -> Option<String> {
    // Ethernet(14) + IPv4(IHL*4) + TCP(data_offset*4).
    const ETH: usize = 14;
    if frame.len() < ETH + 20 + 20 {
        return None;
    }
    if (frame[ETH] >> 4) != 4 {
        return None; // IPv4 only
    }
    let ihl = ((frame[ETH] & 0x0f) as usize) * 4;
    let tcp = ETH + ihl;
    if frame.len() < tcp + 20 {
        return None;
    }
    let data_off = ((frame[tcp + 12] >> 4) as usize) * 4;
    let payload = &frame[tcp + data_off..];
    const METHODS: [&[u8]; 6] = [b"GET ", b"POST ", b"PUT ", b"HEAD ", b"PATCH ", b"HTTP"];
    if !METHODS.iter().any(|m| payload.starts_with(m)) {
        return None;
    }
    let end = payload
        .iter()
        .position(|&b| b == b'\r' || b == b'\n')
        .unwrap_or(payload.len().min(80));
    Some(String::from_utf8_lossy(&payload[..end]).into_owned())
}
