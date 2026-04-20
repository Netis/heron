use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::FmtSubscriber;

use tokenscope::Pipeline;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use ts_common::config::{AppConfig, CaptureSourceConfig, PipelineDef};
use ts_common::internal_metrics::{Metric, MetricsReporter, MetricsSystem};
use ts_storage::create_backend;

#[cfg(feature = "console")]
mod console {
    use axum::http::{header, StatusCode};
    use axum::response::{IntoResponse, Response};
    use rust_embed::Embed;

    #[derive(Embed)]
    #[folder = "../../../console/dist/"]
    pub struct Assets;

    pub async fn static_handler(uri: axum::http::Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        match Assets::get(path) {
            Some(content) => {
                let mime = mime_guess::from_path(path).first_or_octet_stream();
                ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
            }
            None => match Assets::get("index.html") {
                Some(content) => {
                    ([(header::CONTENT_TYPE, "text/html")], content.data).into_response()
                }
                None => StatusCode::NOT_FOUND.into_response(),
            },
        }
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum Color {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Parser)]
#[command(name = "tokenscope", version, about = "LLM API performance monitoring")]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "config/default.toml")]
    config: PathBuf,

    /// Read packets from a pcap file (overrides config pipelines)
    #[arg(long, conflicts_with = "interface")]
    pcap_file: Option<PathBuf>,

    /// Capture from a live network interface (overrides config pipelines)
    #[arg(short = 'i', long)]
    interface: Option<String>,

    /// BPF filter expression for live capture (requires -i)
    #[arg(long, requires = "interface")]
    bpf_filter: Option<String>,

    /// Snapshot length for live capture (only used with -i)
    #[arg(long, default_value = "65535")]
    snaplen: u32,

    /// Increase verbosity level (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Set colored output
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,
}

fn init_logger(color: &Color, verbose: u8) {
    let time_fmt = time::format_description::well_known::Rfc3339;
    let mut builder = FmtSubscriber::builder()
        .with_writer(std::io::stderr)
        .with_timer(UtcTime::new(time_fmt));

    builder = match color {
        Color::Auto => builder.with_ansi(std::io::stderr().is_terminal()),
        Color::Always => builder.with_ansi(true),
        Color::Never => builder.with_ansi(false),
    };

    match verbose {
        0 => builder.with_max_level(LevelFilter::INFO).init(),
        1 => builder.with_max_level(LevelFilter::DEBUG).init(),
        _ => builder.with_max_level(LevelFilter::TRACE).init(),
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    init_logger(&args.color, args.verbose);

    tracing::info!(
        "tokenscope v{} starting, config={}",
        env!("CARGO_PKG_VERSION"),
        args.config.display()
    );

    // Load configuration
    let config = match AppConfig::load(&args.config) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("failed to load config: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!("configuration loaded successfully");

    // Effective pipelines: CLI flags override config pipelines entirely.
    // clap's `conflicts_with` ensures --pcap-file and -i are mutually exclusive.
    let effective_pipelines: Vec<PipelineDef> = if let Some(pcap_file) = &args.pcap_file {
        vec![PipelineDef {
            name: "cli".to_string(),
            sources: vec![CaptureSourceConfig::PcapFile {
                path: pcap_file.to_string_lossy().to_string(),
                realtime: false,
                stream_id: None,
            }],
            ..PipelineDef::default()
        }]
    } else if let Some(interface) = &args.interface {
        vec![PipelineDef {
            name: "cli".to_string(),
            sources: vec![CaptureSourceConfig::Pcap {
                interface: interface.clone(),
                bpf_filter: args.bpf_filter.clone(),
                snaplen: args.snaplen,
                stream_id: None,
            }],
            ..PipelineDef::default()
        }]
    } else {
        config.pipelines.clone()
    };

    // Validate no duplicate stream_ids across all pipeline sources.
    {
        let mut seen = std::collections::HashSet::new();
        for def in &effective_pipelines {
            for cfg in &def.sources {
                if let Some(sid) = cfg.resolved_stream_id() {
                    if !seen.insert(sid.clone()) {
                        tracing::error!("duplicate stream_id '{sid}' across capture sources");
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    tracing::info!("  pipelines: {} configured", effective_pipelines.len());
    for (i, def) in effective_pipelines.iter().enumerate() {
        tracing::info!(
            "    pipeline[{i}] '{}': dispatchers={} flow_shards={} turn_shards={} metrics_shards={} sources={}",
            def.name, def.dispatcher_count, def.flow_shard_count, def.turn.shard_count, def.metrics.shard_count, def.sources.len()
        );
    }
    tracing::info!("  storage: backend={}", config.storage.backend);
    tracing::info!("  api: {}:{}", config.api.listen, config.api.port);
    tracing::info!(
        "  internal_metrics: enabled={}, interval={}s",
        config.internal_metrics.enabled,
        config.internal_metrics.interval_secs
    );

    // Shared cancellation token: capture sources and the retention sweeper
    // both drop out when this fires (Ctrl+C, pipeline failure, etc.).
    let cancel = CancellationToken::new();

    // Initialize storage backend
    let storage = match create_backend(&config.storage) {
        Ok(backend) => backend,
        Err(e) => {
            tracing::error!("failed to create storage backend: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = storage.init().await {
        tracing::error!("failed to initialize storage: {e}");
        std::process::exit(1);
    }
    tracing::info!("storage backend initialized ({})", config.storage.backend);

    // Data retention sweeper (no-op if disabled in config).
    let retention_handle = ts_storage::spawn_retention_task(
        storage.clone(),
        config.storage.retention.clone(),
        cancel.clone(),
    );

    // Bind API server (warn and continue if port is occupied)
    match ts_api::bind(&config.api).await {
        Ok(listener) => {
            let api_storage = storage.clone();
            tokio::spawn(async move {
                let router = ts_api::router(api_storage);
                #[cfg(feature = "console")]
                let router = router.fallback(console::static_handler);
                if let Err(e) = axum::serve(listener, router).await {
                    tracing::error!("API server error: {e}");
                }
            });
        }
        Err(e) => {
            tracing::warn!("API server disabled: {e}");
        }
    }

    if !effective_pipelines.is_empty() && effective_pipelines.iter().any(|d| !d.sources.is_empty())
    {
        // One MetricsSystem per pipeline — the dispatcher/protocol stages
        // register workers against `per_pipeline_metrics[i]` inside
        // `Pipeline::build`, and we start one reporter per system below so
        // log lines are tagged per-pipeline and never merge across pipelines.
        let mut per_pipeline_metrics: Vec<MetricsSystem> = (0..effective_pipelines.len())
            .map(|_| MetricsSystem::new())
            .collect();

        // Pre-register capture metrics for each pipeline's sources.
        let capture_metrics: Vec<Vec<_>> = effective_pipelines
            .iter()
            .zip(per_pipeline_metrics.iter_mut())
            .map(|(def, sys)| {
                def.sources
                    .iter()
                    .enumerate()
                    .map(|(j, _)| {
                        sys.register_worker(
                            &format!("capture.{j}"),
                            &[
                                Metric::CapturePacketsReceived,
                                Metric::CapturePacketsDropped,
                                Metric::CaptureHeartbeatsEmitted,
                                Metric::CaptureBatchesReceived,
                                Metric::CaptureBatchesDropped,
                                Metric::CaptureSourceErrors,
                                Metric::CaptureDumpErrors,
                            ],
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        // Resolve per-pipeline packet-dump configs once; `None` when the
        // pipeline has `pcap_dump.enabled = false` (the default).
        let pipeline_dump_cfgs: Vec<Option<ts_capture::PacketDumperConfig>> = effective_pipelines
            .iter()
            .map(|def| {
                def.pcap_dump
                    .enabled
                    .then(|| ts_capture::PacketDumperConfig::from_config(&def.pcap_dump))
            })
            .collect();

        // Build the pipeline: channels, stages, sink — all wired in one
        // place. `Pipeline::build` registers every per-pipeline stage
        // (including metrics) against the corresponding entry in
        // `per_pipeline_metrics`. There is no cross-pipeline metrics stage.
        let Pipeline {
            pipeline_txs,
            pipeline_sources,
            stage_handles,
        } = Pipeline::build(
            &effective_pipelines,
            &ts_storage::StorageSinkConfig {
                batch_size: config.storage.sink.batch_size,
                flush_interval_ms: config.storage.sink.flush_interval_ms,
            },
            storage.clone(),
            &mut per_pipeline_metrics,
        );

        // Start each per-pipeline MetricsSystem and, if enabled, one
        // reporter per pipeline. Reporter handles are held in a Vec so they
        // stay alive for the duration of the run.
        let _reporter_handles: Vec<_> = per_pipeline_metrics
            .into_iter()
            .zip(effective_pipelines.iter())
            .filter_map(|(sys, def)| {
                let svc = sys.start();
                (config.internal_metrics.enabled && config.internal_metrics.interval_secs > 0).then(
                    || {
                        let label = format!("pipeline.{}", def.name);
                        let handle = MetricsReporter::start(
                            svc,
                            &label,
                            Duration::from_secs(config.internal_metrics.interval_secs),
                        );
                        tracing::info!(
                            "internal metrics reporter started for {label} (interval={}s)",
                            config.internal_metrics.interval_secs
                        );
                        handle
                    },
                )
            })
            .collect();

        // Spawn capture sources — each pipeline may have N sources that
        // fan-in to a single raw-packet channel.
        let mut capture_tasks: JoinSet<()> = JoinSet::new();
        for ((((pipeline_name, routing_tx), source_cfgs), source_metrics), dump_cfg) in
            pipeline_txs
                .into_iter()
                .zip(pipeline_sources.into_iter())
                .zip(capture_metrics.into_iter())
                .zip(pipeline_dump_cfgs.into_iter())
        {
            for ((j, source_cfg), metrics) in source_cfgs
                .iter()
                .enumerate()
                .zip(source_metrics.into_iter())
            {
                let source = match ts_capture::build_source(source_cfg, dump_cfg.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(
                            "failed to build source [{j}] in pipeline '{pipeline_name}': {e}"
                        );
                        std::process::exit(1);
                    }
                };
                let tx = routing_tx.clone();
                let capture_cancel = cancel.clone();
                let pname = pipeline_name.clone();
                capture_tasks.spawn(async move {
                    if let Err(e) = source.run(tx, metrics, capture_cancel).await {
                        tracing::error!("capture source [{j}] in pipeline '{pname}' error: {e}");
                    }
                });
            }
            // Drop our clone — spawned tasks hold theirs.
        }

        // Wait for: ctrl-c, all capture sources finishing, or any pipeline
        // stage task panicking. Any of the three triggers shutdown. The
        // storage sink is part of `stage_handles`, so `supervisor` also
        // observes its final drain.
        let mut supervisor = tokio::spawn(Pipeline::supervise(stage_handles));
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl+C, stopping...");
                cancel.cancel();
            }
            _ = async {
                while capture_tasks.join_next().await.is_some() {}
            } => {
                tracing::debug!("all capture sources finished");
            }
            res = &mut supervisor => {
                match res {
                    Ok(Some((label, err))) => tracing::error!(
                        "pipeline stage '{label}' exited abnormally: {err}; cancelling capture"
                    ),
                    Ok(None) => tracing::debug!("all pipeline stages exited cleanly"),
                    Err(e) => tracing::error!("supervisor join error: {e}"),
                }
                cancel.cancel();
            }
        }

        // Wait for any remaining capture tasks briefly, then await pipeline drain.
        tokio::select! {
            _ = async {
                while capture_tasks.join_next().await.is_some() {}
            } => {}
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                tracing::warn!("capture source(s) did not stop in time; aborting");
                capture_tasks.abort_all();
            }
        }

        tracing::info!("waiting for pipeline (incl. storage sink) to drain...");
        match supervisor.await {
            Ok(Some((label, err))) => {
                tracing::error!("pipeline stage '{label}' exited abnormally: {err}")
            }
            Ok(None) => tracing::debug!("all pipeline stages drained cleanly"),
            Err(e) => tracing::error!("supervisor join error: {e}"),
        }
        tracing::info!("pipeline drained");
    } else {
        // No pipelines with sources → no pipeline, no MetricsSystem, no
        // reporter. Just park on ctrl-c so the API server stays up.
        tracing::info!(
            "no pipelines with sources configured (use --pcap-file, -i, or [[pipeline]] in config)"
        );
        tracing::info!("tokenscope ready, press Ctrl+C to stop");

        match tokio::signal::ctrl_c().await {
            Ok(()) => tracing::info!("received Ctrl+C, shutting down..."),
            Err(e) => tracing::error!("failed to listen for Ctrl+C: {e}"),
        }
        cancel.cancel();
    }

    // Let the retention sweeper observe cancellation and exit cleanly.
    if let Err(e) = retention_handle.await {
        tracing::warn!("retention task join error: {e}");
    }

    tracing::info!("tokenscope stopped");
}
