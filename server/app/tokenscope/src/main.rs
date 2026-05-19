use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::FmtSubscriber;

use tokenscope::Pipeline;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use ts_common::config::{
    config_search_paths, discover_config_path, AppConfig, CaptureSourceConfig, PipelineDef,
};
use ts_common::internal_metrics::{Metric, MetricsReporter, MetricsSystem};
use tokenscope::create_backend;

mod cmd;

const CAPTURE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const PIPELINE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const API_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const RETENTION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(feature = "console")]
pub(crate) mod console {
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
pub(crate) enum Color {
    Auto,
    Always,
    Never,
}

/// Top-level CLI. When invoked without a subcommand, runs the capture
/// pipeline (existing behavior). Subcommands `config validate` and `doctor`
/// reserve those names — the flag-only API has no positional collision.
#[derive(Debug, Parser)]
#[command(name = "tokenscope", version, about = "LLM API performance monitoring")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,

    #[command(flatten)]
    run: RunArgs,

    /// Path to configuration file. When omitted, TokenScope searches
    /// (in order): ./config/default.toml, $XDG_CONFIG_HOME/tokenscope/config.toml,
    /// ~/.config/tokenscope/config.toml, /etc/tokenscope/config.toml.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Increase verbosity level (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Set colored output
    #[arg(long, value_enum, default_value_t = Color::Auto, global = true)]
    color: Color,
}

/// Capture-pipeline args. Lives on the root `Cli` (via `flatten`) so the
/// existing `tokenscope -i eth0` / `tokenscope --pcap-file foo.pcap`
/// invocations keep working unchanged when no subcommand is given.
#[derive(Debug, Args)]
struct RunArgs {
    /// Read packets from a pcap file (overrides config pipelines).
    /// Typically combine with --no-retention so historical timestamps in the
    /// pcap aren't pruned by the retention sweeper.
    #[arg(long, conflicts_with = "interface")]
    pcap_file: Option<PathBuf>,

    /// Capture from a live network interface (overrides config pipelines)
    #[arg(short = 'i', long)]
    interface: Option<String>,

    /// BPF filter expression for live capture (requires -i)
    #[arg(long, requires = "interface")]
    bpf_filter: Option<String>,

    /// Snapshot length for live capture (only used with -i)
    #[arg(long, default_value = "262144")]
    snaplen: u32,

    /// Exit when capture sources finish and the pipeline drains (batch mode).
    /// Default: keep the API/console available; press Ctrl+C to exit.
    #[arg(long)]
    exit_after_drain: bool,

    /// Disable the retention sweeper for this run (overrides config).
    /// Useful with --pcap-file when the pcap's event timestamps are older than
    /// the retention window — without this, freshly imported data is pruned by
    /// the next sweep.
    #[arg(long)]
    no_retention: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect or validate the configuration file.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Run pre-flight diagnostics (config, capture privileges, storage, API port, console).
    Doctor(cmd::doctor::DoctorArgs),
    /// Re-tokenize historical rows whose `usage` block was missing on the wire,
    /// filling in input_tokens / output_tokens / total_tokens via cl100k.
    /// Stop the live tokenscope daemon first — DuckDB takes an exclusive lock.
    BackfillTokens(cmd::backfill_tokens::BackfillTokensArgs),
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Validate the configuration file without starting any pipelines.
    Validate(cmd::validate::ValidateArgs),
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

/// Resolve on the first of Ctrl+C, SIGTERM, or SIGHUP. Returns the signal
/// name so the caller can log which one fired. `tmux kill-session` (used by
/// `just demo stop`) sends SIGHUP — without catching it, Rust's default
/// aborts the process before Drop runs, leaving pcap dumps truncated.
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
    "Ctrl+C"
}

async fn join_capture_tasks(capture_tasks: &mut JoinSet<()>) {
    while capture_tasks.join_next().await.is_some() {}
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.cmd {
        Some(Command::Config {
            cmd: ConfigCmd::Validate(args),
        }) => {
            let code = cmd::validate::run(cli.config.as_deref(), &args);
            std::process::exit(code);
        }
        Some(Command::Doctor(args)) => {
            let code = cmd::doctor::run(cli.config.as_deref(), &args).await;
            std::process::exit(code);
        }
        Some(Command::BackfillTokens(args)) => {
            init_logger(&cli.color, cli.verbose);
            let code = cmd::backfill_tokens::run(&args);
            std::process::exit(code);
        }
        None => {
            run_pipeline(cli).await;
        }
    }
}

async fn run_pipeline(cli: Cli) {
    init_logger(&cli.color, cli.verbose);

    let config_path: PathBuf = match cli.config {
        Some(p) => {
            if !p.is_file() {
                tracing::error!("config file not found: '{}'", p.display());
                std::process::exit(1);
            }
            p
        }
        None => match discover_config_path() {
            Some(p) => p,
            None => {
                tracing::error!("no configuration file found. Searched (in order):");
                for p in config_search_paths() {
                    tracing::error!("  - {}", p.display());
                }
                tracing::error!(
                    "Run install.sh to drop a default config, or pass -c <path> explicitly."
                );
                std::process::exit(1);
            }
        },
    };

    tracing::info!(
        "tokenscope v{} starting, config={}",
        env!("CARGO_PKG_VERSION"),
        config_path.display()
    );

    let mut config = match AppConfig::load(&config_path) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("failed to load config '{}': {e}", config_path.display());
            std::process::exit(1);
        }
    };
    let loaded_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    tracing::info!("configuration loaded successfully");

    if cli.run.no_retention && config.storage.retention.enabled {
        tracing::info!("retention disabled by --no-retention CLI flag");
        config.storage.retention.enabled = false;
    }

    // Effective pipelines: CLI flags override config pipelines entirely.
    // clap's `conflicts_with` ensures --pcap-file and -i are mutually exclusive.
    let effective_pipelines: Vec<PipelineDef> = if let Some(pcap_file) = &cli.run.pcap_file {
        vec![PipelineDef {
            name: "cli".to_string(),
            sources: vec![CaptureSourceConfig::PcapFile {
                path: pcap_file.to_string_lossy().to_string(),
                realtime: false,
                source_id: None,
            }],
            ..PipelineDef::default()
        }]
    } else if let Some(interface) = &cli.run.interface {
        vec![PipelineDef {
            name: "cli".to_string(),
            sources: vec![CaptureSourceConfig::Pcap {
                interface: interface.clone(),
                bpf_filter: cli.run.bpf_filter.clone(),
                snaplen: cli.run.snaplen,
                source_id: None,
            }],
            ..PipelineDef::default()
        }]
    } else {
        config.pipelines.clone()
    };

    let pcap_extract_roots: std::sync::Arc<Vec<ts_pcap_extract::PipelineRoot>> =
        std::sync::Arc::new(
            effective_pipelines
                .iter()
                .map(|def| ts_pcap_extract::PipelineRoot {
                    name: def.name.clone(),
                    dump_dir: std::path::PathBuf::from(&def.pcap_dump.dir),
                })
                .collect(),
        );

    // Validate no duplicate source_ids across all pipeline sources.
    {
        let mut seen = std::collections::HashSet::new();
        for def in &effective_pipelines {
            for cfg in &def.sources {
                if let Some(sid) = cfg.resolved_source_id() {
                    if !seen.insert(sid.clone()) {
                        tracing::error!("duplicate source_id '{sid}' across capture sources");
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    // Snapshot of the configuration the process is *actually* running with —
    // pipelines reflect CLI overrides (`--pcap-file`, `-i`), everything else
    // mirrors `AppConfig::load` (which already absorbed `TS_*` env overrides).
    // Exposed read-only via `GET /api/runtime-config`.
    let runtime_config_ctx = ts_api::ApiRuntimeConfigContext {
        config: Arc::new(AppConfig {
            pipelines: effective_pipelines.clone(),
            storage: config.storage.clone(),
            internal_metrics: config.internal_metrics.clone(),
            api: config.api.clone(),
            proxy: config.proxy.clone(),
        }),
        config_path: config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.clone())
            .to_string_lossy()
            .into_owned(),
        loaded_at_ms,
        version: env!("CARGO_PKG_VERSION"),
    };

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

    // Pipeline-drained signal exposed to `/api/health`. Stays `false` while
    // sources are feeding the pipeline; flipped to `true` after the pipeline
    // has drained and the process is parked waiting for a shutdown signal
    // (the keep-the-API-up default for `--pcap-file` replay).
    let drained = Arc::new(AtomicBool::new(false));

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

    // Pcap-dump file retention: one task per pipeline that has dumping
    // enabled. Spawned inside the per-pipeline setup block below; declared
    // here so the shutdown path can join the handles.
    let mut pcap_retention_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Bind API server early so port-bind errors abort startup fast. The actual
    // `axum::serve` spawn happens after the per-pipeline + global
    // `MetricsSystem::start()` calls below so the API's `ApiMetricsContext`
    // carries fully populated `Arc<MetricsSvc>`s.
    let mut api_listener: Option<tokio::net::TcpListener> = match ts_api::bind(&config.api).await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::error!("failed to bind API server: {e}");
            std::process::exit(1);
        }
    };
    let mut api_handle: Option<tokio::task::JoinHandle<()>> = None;

    if !effective_pipelines.is_empty() && effective_pipelines.iter().any(|d| !d.sources.is_empty())
    {
        // One MetricsSystem per pipeline — the dispatcher/protocol stages
        // register workers against `per_pipeline_metrics[i]` inside
        // `Pipeline::build`, and we start one reporter per system below so
        // log lines are tagged per-pipeline and never merge across pipelines.
        let mut per_pipeline_metrics: Vec<MetricsSystem> = (0..effective_pipelines.len())
            .map(|_| MetricsSystem::new())
            .collect();

        // Cross-pipeline counters (storage sink + storage queue probes) live
        // here so they show up under a separate `global` reporter rather than
        // being mis-attributed to the first pipeline's reporter line.
        let mut shared_metrics = MetricsSystem::new();

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
                                Metric::CaptureKernelPacketsDropped,
                                Metric::CaptureTruncatedPackets,
                                Metric::CaptureHeartbeatsEmitted,
                                Metric::CaptureBatchesReceived,
                                Metric::CaptureZmqBatchesDropped,
                                Metric::CaptureReadErrors,
                                Metric::CaptureDumpErrors,
                                Metric::CaptureDumpLateMinutePackets,
                            ],
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        // Resolve per-pipeline packet-dump configs once; `None` when the
        // pipeline has `pcap_dump.enabled = false` (the default) or when
        // its name produces an unsafe path component. The configured
        // `pcap_dump.dir` is the *base*; we always append a sanitized
        // pipeline-name layer so multiple pipelines that happen to share
        // a base dir stay fully isolated on disk.
        let pipeline_dump_cfgs: Vec<Option<ts_capture::PacketDumperConfig>> = effective_pipelines
            .iter()
            .map(|def| {
                if !def.pcap_dump.enabled {
                    return None;
                }
                let base = PathBuf::from(&def.pcap_dump.dir);
                let resolved = match ts_capture::pcap_dump_dir_for(&base, &def.name) {
                    Some(p) => p,
                    None => {
                        tracing::error!(
                            pipeline = %def.name,
                            "pcap_dump: pipeline name is not a safe path component; disabling dump for this pipeline"
                        );
                        return None;
                    }
                };
                Some(ts_capture::PacketDumperConfig {
                    dir: resolved,
                    compression: def.pcap_dump.compression,
                })
            })
            .collect();

        // Spawn one pcap-dump retention task per pipeline that has dumping
        // enabled. Each task scans only its own pipeline's subtree (same
        // `<dir>/<pipeline>` path the dumper writes to) — this is what
        // keeps multiple pipelines using a shared base dir isolated:
        // tasks never see each other's files. Retention workers are
        // registered against the same per-pipeline `MetricsSystem` so
        // counters land in that pipeline's reporter line.
        for ((def, sys), dump_cfg) in effective_pipelines
            .iter()
            .zip(per_pipeline_metrics.iter_mut())
            .zip(pipeline_dump_cfgs.iter())
        {
            let Some(dump_cfg) = dump_cfg else {
                continue;
            };
            let worker = sys.register_worker(
                "pcap_retention",
                &[
                    Metric::CaptureDumpRetentionFilesDeleted,
                    Metric::CaptureDumpRetentionBytesDeleted,
                    Metric::CaptureDumpRetentionErrors,
                ],
            );
            pcap_retention_handles.push(ts_capture::spawn_pcap_retention_task(
                dump_cfg.dir.clone(),
                def.pcap_dump.retention.clone(),
                worker,
                cancel.clone(),
            ));
        }

        // Build the pipeline: channels, stages, sink — all wired in one
        // place. `Pipeline::build` registers per-pipeline stage workers
        // against the corresponding entry in `per_pipeline_metrics`, and
        // shared (storage sink + queue probes) workers against
        // `shared_metrics`. There is no cross-pipeline metrics stage.
        // In-memory registry of in-progress agent turns. Shared by every
        // turn-tracker shard (writers) and the API's /api/agent-turns
        // handler (reader) so the console sees in-progress turns alongside
        // finalized ones without DB write amplification.
        let active_turns = ts_turn::new_active_turn_registry();

        let Pipeline {
            pipeline_txs,
            pipeline_sources,
            stage_handles,
            proxy_joiner_tx,
        } = Pipeline::build(
            &effective_pipelines,
            &ts_storage::StorageSinkConfig {
                batch_size: config.storage.sink.batch_size,
                flush_interval_ms: config.storage.sink.flush_interval_ms,
            },
            storage.clone(),
            &mut per_pipeline_metrics,
            &mut shared_metrics,
            active_turns.clone(),
        );

        // Start each per-pipeline MetricsSystem and, if enabled, one
        // reporter per pipeline. Per-pipeline and global handles are kept
        // separate so shutdown can stage them: per-pipeline reporters drain
        // first (their final ticks print), then `global` — making the global
        // storage summary the last block of metrics output.
        let reporter_enabled =
            config.internal_metrics.enabled && config.internal_metrics.interval_secs > 0;
        let mut api_pipeline_metrics: Vec<(
            String,
            std::sync::Arc<ts_common::internal_metrics::MetricsSvc>,
        )> = Vec::new();
        let pipeline_reporter_handles: Vec<_> = per_pipeline_metrics
            .into_iter()
            .zip(effective_pipelines.iter())
            .filter_map(|(sys, def)| {
                let svc = sys.start();
                api_pipeline_metrics.push((def.name.clone(), svc.clone()));
                reporter_enabled.then(|| {
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
                })
            })
            .collect();

        // Cross-pipeline reporter — the storage sink + its queue probes are
        // the only workers registered here, so the log line is honest about
        // representing every pipeline's traffic into a single shared sink.
        let api_global_metrics = shared_metrics.start();
        let global_reporter_handle = {
            let svc = api_global_metrics.clone();
            reporter_enabled.then(|| {
                let handle = MetricsReporter::start(
                    svc,
                    "global",
                    Duration::from_secs(config.internal_metrics.interval_secs),
                );
                tracing::info!(
                    "internal metrics reporter started for global (interval={}s)",
                    config.internal_metrics.interval_secs
                );
                handle
            })
        };

        // Now that every per-pipeline + global `MetricsSvc` exists, spawn the
        // API server with a fully populated `ApiMetricsContext`. We assign to
        // the previously-declared `mut api_handle` (no `let`) so the shutdown
        // logic below sees the JoinHandle. Use `.take()` so the listener stays
        // None after this arm — the outer `if api_handle.is_none()` fallback
        // (further below) sees the listener is consumed and is a no-op.
        api_handle = match api_listener.take() {
            Some(listener) => {
                let api_storage = storage.clone();
                let api_cancel = cancel.clone();
                let pipeline_names: Vec<String> = api_pipeline_metrics
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect();
                let api_metrics = ts_api::ApiMetricsContext {
                    pipelines: api_pipeline_metrics,
                    global: api_global_metrics.clone(),
                };
                let api_runtime_config = runtime_config_ctx.clone();
                let api_health = ts_api::ApiHealthContext {
                    started_at_ms: loaded_at_ms,
                    version: env!("CARGO_PKG_VERSION"),
                    pipelines: pipeline_names,
                    drained: drained.clone(),
                };
                let pcap_extract_roots = pcap_extract_roots.clone();
                let api_active_turns = active_turns.clone();
                Some(tokio::spawn(async move {
                    let router = ts_api::router(
                        api_storage,
                        api_metrics,
                        api_runtime_config,
                        api_health,
                        pcap_extract_roots,
                        api_active_turns,
                    );
                    #[cfg(feature = "console")]
                    let router = router.fallback(console::static_handler);
                    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                        api_cancel.cancelled().await;
                    });
                    if let Err(e) = server.await {
                        tracing::error!("API server error: {e}");
                    } else {
                        tracing::info!("API server stopped");
                    }
                }))
            }
            None => None,
        };

        // Built-in MITM forward proxy — optional, off by default.
        // Hooks into the same storage path as sniffed traffic by
        // injecting `HttpJoinerEvent::Exchange` directly into the first
        // pipeline's joiner channel. The proxy task itself runs until
        // the listener errors out hard; graceful shutdown piggybacks on
        // process exit (no flush-state to drain).
        let _proxy_task = if config.proxy.enabled {
            match proxy_joiner_tx.clone() {
                Some(joiner_tx) => {
                    let proxy_config = config.proxy.clone();
                    let deps = ts_proxy::ProxyDeps {
                        joiner_event_tx: Some(joiner_tx),
                        upstream: ts_proxy::UpstreamClient::with_webpki_roots(),
                    };
                    match ts_proxy::spawn_proxy(proxy_config.clone(), deps).await {
                        Ok((handle, bound)) => {
                            tracing::info!(
                                "MITM proxy listening on {} (ca_dir = {}, redact = {:?})",
                                bound,
                                proxy_config.ca_dir,
                                proxy_config.redact_api_keys
                            );
                            Some(handle)
                        }
                        Err(e) => {
                            tracing::error!("failed to start MITM proxy: {e}");
                            None
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        "proxy.enabled = true but no pipelines configured — \
                         captured exchanges would have nowhere to land; not starting proxy"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Spawn capture sources — each pipeline may have N sources that
        // fan-in to a single raw-packet channel.
        let mut capture_tasks: JoinSet<()> = JoinSet::new();
        for ((((pipeline_name, routing_tx), source_cfgs), source_metrics), dump_cfg) in pipeline_txs
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
        // stage task panicking. Only the signal arm cancels eagerly — the
        // other two arms let the pipeline drain naturally (`Pipeline::supervise`
        // returns when every stage's upstream sender drops) and fall through
        // to the post-drain park below, so the API/console stays available
        // for inspection. `--exit-after-drain` short-circuits the park.
        let mut supervisor = tokio::spawn(Pipeline::supervise(stage_handles));
        tokio::select! {
            sig = wait_shutdown_signal() => {
                tracing::info!("received {sig}, stopping...");
                cancel.cancel();
            }
            _ = async {
                while capture_tasks.join_next().await.is_some() {}
            } => {
                tracing::info!("all capture sources finished");
            }
            res = &mut supervisor => {
                match res {
                    Ok(Some((label, err))) => tracing::error!(
                        "pipeline stage '{label}' exited abnormally: {err}"
                    ),
                    Ok(None) => tracing::info!("all pipeline stages exited cleanly"),
                    Err(e) => tracing::error!("supervisor join error: {e}"),
                }
            }
        }

        // NOTE: reporters keep running through capture-stop and pipeline-drain
        // so their final-tick numbers reflect the truly drained state
        // (`flushed_* == buf_*`, `q_* == 0`). They get the stop signal *after*
        // drain at the bottom of this block.

        let mut force_exit = false;

        // Graceful capture shutdown is best-effort. If a blocking capture
        // worker ignores cancellation and keeps a RawPacket sender alive,
        // the pipeline can never observe EOF; after the timeout we stop
        // waiting and force the whole process down below.
        if tokio::time::timeout(
            CAPTURE_SHUTDOWN_TIMEOUT,
            join_capture_tasks(&mut capture_tasks),
        )
        .await
        .is_err()
        {
            tracing::error!(
                timeout_secs = CAPTURE_SHUTDOWN_TIMEOUT.as_secs(),
                "capture source(s) did not stop in time; forcing shutdown"
            );
            capture_tasks.abort_all();
            force_exit = true;
        }

        tracing::info!("waiting for pipeline (incl. storage sink) to drain...");
        let pipeline_drained =
            match tokio::time::timeout(PIPELINE_DRAIN_TIMEOUT, &mut supervisor).await {
                Ok(Ok(Some((label, err)))) => {
                    tracing::error!("pipeline stage '{label}' exited abnormally: {err}");
                    true
                }
                Ok(Ok(None)) => {
                    tracing::info!("all pipeline stages drained cleanly");
                    true
                }
                Ok(Err(e)) => {
                    tracing::error!("supervisor join error: {e}");
                    true
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = PIPELINE_DRAIN_TIMEOUT.as_secs(),
                        "pipeline drain timed out; forcing shutdown"
                    );
                    force_exit = true;
                    false
                }
            };
        if pipeline_drained {
            tracing::info!("pipeline drained");
        }

        // Stop reporters now that drain is done — their final tick captures
        // post-drain totals (every `flushed_*` should equal its `buf_*`).
        // Stage the shutdown so `global` is logged *last*: signal every
        // per-pipeline reporter, await each task's exit (its final tick has
        // been logged by then), and only then signal `global` and await it.
        for handle in &pipeline_reporter_handles {
            let _ = handle.stop_tx.send(());
        }
        for handle in pipeline_reporter_handles {
            let _ = handle.join.await;
        }
        if let Some(handle) = global_reporter_handle {
            let _ = handle.stop_tx.send(());
            let _ = handle.join.await;
        }

        // Park on a shutdown signal so the API/console stays available for
        // post-drain inspection — unless the user opted into batch mode with
        // `--exit-after-drain`, or a real signal already fired `cancel` (in
        // which case the API is already on its way down).
        if !cancel.is_cancelled() {
            drained.store(true, Ordering::Release);
            if cli.run.exit_after_drain {
                tracing::info!("pipeline drained; --exit-after-drain set, exiting");
                cancel.cancel();
            } else {
                tracing::info!(
                    "pipeline drained; API/console remains available — press Ctrl+C to exit \
                     (use --exit-after-drain for batch mode)"
                );
                let sig = wait_shutdown_signal().await;
                tracing::info!("received {sig}, shutting down...");
                cancel.cancel();
            }
        }

        if force_exit {
            tracing::error!("graceful shutdown stalled; exiting forcefully");
            std::process::exit(1);
        }
    } else {
        // No pipelines with sources → no pipeline, no MetricsSystem, no
        // reporter. Spawn the API server (with empty metrics contexts) so
        // /api/server-info and storage-backed routes still serve, then park
        // on ctrl-c so the API stays up. /api/internal-metrics returns
        // empty arrays. Guarded by `is_none()` so we never double-spawn —
        // when the pipeline arm ran it already consumed the listener via
        // `.take()` and set `api_handle = Some(_)`.
        if api_handle.is_none() {
            api_handle = match api_listener.take() {
                Some(listener) => {
                    let api_storage = storage.clone();
                    let api_cancel = cancel.clone();
                    let empty_global = MetricsSystem::new().start();
                    let api_metrics = ts_api::ApiMetricsContext {
                        pipelines: Vec::new(),
                        global: empty_global,
                    };
                    let api_runtime_config = runtime_config_ctx.clone();
                    let api_health = ts_api::ApiHealthContext {
                        started_at_ms: loaded_at_ms,
                        version: env!("CARGO_PKG_VERSION"),
                        pipelines: Vec::new(),
                        drained: drained.clone(),
                    };
                    let pcap_extract_roots = pcap_extract_roots.clone();
                    // No pipelines were configured ⇒ no tracker is running ⇒
                    // the registry stays empty for the lifetime of this
                    // process. Construct a fresh empty one so the API still
                    // serves /api/agent-turns (returning DB rows only).
                    let api_active_turns = ts_turn::new_active_turn_registry();
                    Some(tokio::spawn(async move {
                        let router = ts_api::router(
                            api_storage,
                            api_metrics,
                            api_runtime_config,
                            api_health,
                            pcap_extract_roots,
                            api_active_turns,
                        );
                        #[cfg(feature = "console")]
                        let router = router.fallback(console::static_handler);
                        let server =
                            axum::serve(listener, router).with_graceful_shutdown(async move {
                                api_cancel.cancelled().await;
                            });
                        if let Err(e) = server.await {
                            tracing::error!("API server error: {e}");
                        } else {
                            tracing::info!("API server stopped");
                        }
                    }))
                }
                None => None,
            };
        }

        tracing::info!(
            "no pipelines with sources configured (use --pcap-file, -i, or [[pipeline]] in config)"
        );
        tracing::info!("tokenscope ready, press Ctrl+C to stop");

        let sig = wait_shutdown_signal().await;
        tracing::info!("received {sig}, shutting down...");
        cancel.cancel();
    }

    if let Some(api_handle) = api_handle {
        match tokio::time::timeout(API_SHUTDOWN_TIMEOUT, api_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("API server task join error: {e}"),
            Err(_) => {
                tracing::error!(
                    timeout_secs = API_SHUTDOWN_TIMEOUT.as_secs(),
                    "API server did not stop in time; exiting forcefully"
                );
                std::process::exit(1);
            }
        }
    }

    // Let the retention sweeper observe cancellation and exit cleanly.
    match tokio::time::timeout(RETENTION_SHUTDOWN_TIMEOUT, retention_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("retention task join error: {e}"),
        Err(_) => {
            tracing::error!(
                timeout_secs = RETENTION_SHUTDOWN_TIMEOUT.as_secs(),
                "retention task did not stop in time; exiting forcefully"
            );
            std::process::exit(1);
        }
    }

    // Same treatment for each pcap-dump retention task. Per-handle timeout
    // (not a shared budget) — each task exits within one `select!` poll,
    // so the total worst case is N * timeout but in practice << N seconds.
    for h in pcap_retention_handles {
        match tokio::time::timeout(RETENTION_SHUTDOWN_TIMEOUT, h).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("pcap retention task join error: {e}"),
            Err(_) => {
                tracing::error!(
                    timeout_secs = RETENTION_SHUTDOWN_TIMEOUT.as_secs(),
                    "pcap retention task did not stop in time; exiting forcefully"
                );
                std::process::exit(1);
            }
        }
    }

    tracing::info!("tokenscope stopped");
}
