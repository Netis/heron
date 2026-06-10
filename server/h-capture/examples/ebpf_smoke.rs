//! Live eBPF capture smoke test.
//!
//! Runs the eBPF `SSL_read`/`SSL_write` capture source and prints the HTTP
//! request lines it reconstructs from synthesized frames. Proves the
//! uprobe → ring buffer → frame-synthesis path against real `libssl` traffic
//! without the rest of the pipeline.
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
    let config = CaptureSourceConfig::Ebpf {
        source_id: Some("ebpf-smoke".to_string()),
        ssl_libs: vec![],     // autodetect
        targets: vec![],
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
                if let Some(line) = http_first_line(&pkt.data) {
                    req_lines += 1;
                    println!("  [{pkts:>4}] {line}");
                    if req_lines >= 8 {
                        break;
                    }
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
