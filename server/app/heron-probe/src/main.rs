//! `heron-probe` — the edge agent of Heron's distributed eBPF topology.
//!
//! Runs one capture source (an `ebpf` SSL-uprobe source on a Linux edge host, a
//! `pcap-file` source for dev smoke) and ships the captured `RawPacket`s —
//! process attribution included — to a central `heron` over mTLS. The heavy,
//! frequently-changing wire-API decoding stays central; the probe is small and
//! rarely needs upgrading. See `h-capture/src/{wire,thin_probe,probe_uplink}.rs`.

mod config;

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use h_capture::{build_source, ProbeUplink, RawPacket, RoutingSender};
use h_common::internal_metrics::{Metric, MetricsSystem};

use crate::config::ProbeConfig;

/// The Capture*/Ebpf* counters a capture source touches. Registered up front so
/// the source never panics writing to an unregistered metric (counter() panics
/// on a missing registration — see h-common/src/internal_metrics.rs).
const CAPTURE_METRICS: &[Metric] = &[
    Metric::CapturePacketsReceived,
    Metric::CaptureKernelPacketsDropped,
    Metric::CaptureTruncatedPackets,
    Metric::CaptureHeartbeatsEmitted,
    Metric::CaptureBatchesReceived,
    Metric::CaptureZmqBatchesDropped,
    Metric::CaptureReadErrors,
    Metric::CaptureDumpErrors,
    Metric::CaptureDumpLateMinutePackets,
    Metric::EbpfEventsReceived,
    Metric::EbpfEventsDropped,
    Metric::EbpfBytesCaptured,
    Metric::EbpfFramesSynthesized,
    Metric::EbpfUprobesAttached,
    Metric::EbpfConnectionsActive,
    Metric::EbpfProcessCacheSize,
];

#[derive(Parser, Debug)]
#[command(
    name = "heron-probe",
    about = "Heron edge capture probe (eBPF → mTLS uplink)"
)]
struct Cli {
    /// Path to the probe TOML config.
    #[arg(short, long, default_value = "heron-probe.toml")]
    config: PathBuf,
    /// Increase log verbosity (-v debug, -vv trace). Overridden by RUST_LOG.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let cfg = match ProbeConfig::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "heron-probe: failed to load config {}: {e}",
                cli.config.display()
            );
            std::process::exit(2);
        }
    };

    if let Err(e) = run(cfg).await {
        tracing::error!("heron-probe: fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(cfg: ProbeConfig) -> Result<(), String> {
    let cancel = CancellationToken::new();

    // Build the capture source via the shared factory. On a build without the
    // `ebpf` feature, an `ebpf` source fails here with a clear message rather
    // than silently capturing nothing.
    let source =
        build_source(&cfg.source, None).map_err(|e| format!("build capture source: {e}"))?;

    let mut metrics_sys = MetricsSystem::new();
    let capture_metrics = metrics_sys.register_worker("capture", CAPTURE_METRICS);

    // Resolve the probe identity. HERON_PROBE_SOURCE_ID (e.g. the K8s node name
    // injected via the Downward API in the DaemonSet) overrides the config file;
    // an empty result means the central falls back to our client-cert CN.
    let source_id = std::env::var("HERON_PROBE_SOURCE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.source_id.clone())
        .unwrap_or_default();

    let (tx, rx) = mpsc::channel::<RawPacket>(cfg.queue_capacity);
    let cancel_src = cancel.clone();
    let capture_task = tokio::spawn(async move {
        if let Err(e) = source
            .run(RoutingSender::single(tx), capture_metrics, cancel_src)
            .await
        {
            tracing::error!("heron-probe: capture source failed: {e}");
        }
    });

    let uplink = ProbeUplink::from_config(
        cfg.central_endpoint.clone(),
        cfg.server_name.clone(),
        source_id.clone(),
        &cfg.tls,
    )
    .map_err(|e| format!("build uplink: {e}"))?
    .with_batching(
        cfg.batching.max_packets,
        Duration::from_millis(cfg.batching.flush_ms),
    );
    let cancel_up = cancel.clone();
    let uplink_task = tokio::spawn(async move { uplink.run(rx, cancel_up).await });

    tracing::info!(
        central = %cfg.central_endpoint,
        source_id = if source_id.is_empty() { "<client-cert CN>" } else { &source_id },
        "heron-probe: started",
    );

    let sig = wait_shutdown_signal().await;
    tracing::info!("heron-probe: received {sig}, shutting down");
    cancel.cancel();
    let _ = capture_task.await;
    let _ = uplink_task.await;
    tracing::info!("heron-probe: stopped");
    Ok(())
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Resolve on the first of Ctrl+C, SIGTERM, or SIGHUP.
#[cfg(unix)]
async fn wait_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = term.recv() => "SIGTERM",
        _ = hup.recv() => "SIGHUP",
    }
}

#[cfg(not(unix))]
async fn wait_shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "SIGINT"
}
