use std::collections::HashMap;
use std::path::{Path, PathBuf};

use config::Config;
use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// The ordered list of paths searched for a configuration file when the user
/// does not pass `-c <path>` explicitly.
///
/// Order is significant: earlier entries override later ones. The cascade is:
///
/// 1. `./config/default.toml` — development mode (`cargo run` from the repo;
///    or the layout inside an extracted release tarball).
/// 2. `$XDG_CONFIG_HOME/tokenscope/config.toml` — user override (XDG-aware).
/// 3. `~/.config/tokenscope/config.toml` — user override (XDG default).
/// 4. `/etc/tokenscope/config.toml` — system-wide install (dropped by
///    `install.sh` when invoked with `sudo`).
///
/// On macOS we deliberately use the same `~/.config/` location as Linux —
/// the major modern CLI tools (gh, ripgrep, fd, bat, helix) follow this
/// convention rather than `~/Library/Application Support/`.
pub fn config_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(4);
    paths.push(PathBuf::from("config/default.toml"));

    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            paths.push(PathBuf::from(xdg).join("tokenscope/config.toml"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            paths.push(PathBuf::from(home).join(".config/tokenscope/config.toml"));
        }
    }
    paths.push(PathBuf::from("/etc/tokenscope/config.toml"));

    paths
}

/// Walk [`config_search_paths`] and return the first path that exists as a
/// regular file. Returns `None` when no config is found anywhere — callers
/// should print the searched paths so the user knows what to fix.
pub fn discover_config_path() -> Option<PathBuf> {
    config_search_paths().into_iter().find(|p| p.is_file())
}

/// Top-level application configuration.
///
/// Not directly deserializable — use [`AppConfig::load`] or [`AppConfig::from_toml`]
/// which go through [`RawAppConfig`] two-phase parsing.
#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    pub pipelines: Vec<PipelineDef>,
    pub storage: StorageConfig,
    pub internal_metrics: InternalMetricsConfig,
    pub api: ApiConfig,
}

/// A single pipeline definition bundling sources and pipeline parameters.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PipelineDef {
    #[serde(default = "default_pipeline_name")]
    pub name: String,
    #[serde(default = "default_dispatcher_count")]
    pub dispatcher_count: usize,
    #[serde(default = "default_flow_shard_count")]
    pub flow_shard_count: usize,
    #[serde(default)]
    pub queues: QueueConfig,
    #[serde(default)]
    pub turn: TurnConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub pcap_dump: PcapDumpConfig,
    #[serde(default)]
    pub sources: Vec<CaptureSourceConfig>,
}

impl Default for PipelineDef {
    fn default() -> Self {
        Self {
            name: default_pipeline_name(),
            dispatcher_count: default_dispatcher_count(),
            flow_shard_count: default_flow_shard_count(),
            queues: QueueConfig::default(),
            turn: TurnConfig::default(),
            metrics: MetricsConfig::default(),
            pcap_dump: PcapDumpConfig::default(),
            sources: Vec::new(),
        }
    }
}

fn default_dispatcher_count() -> usize {
    1
}

fn default_pipeline_name() -> String {
    "default".to_string()
}

/// Intermediate struct for two-phase TOML deserialization.
/// Supports both old `[pipeline]` + `[[capture.sources]]` format
/// and new `[[pipeline]]` array format.
#[derive(Deserialize)]
struct RawAppConfig {
    #[serde(default)]
    capture: Option<CaptureConfig>,
    #[serde(default)]
    pipeline: Option<RawPipeline>,
    #[serde(default)]
    storage: StorageConfig,
    #[serde(default)]
    internal_metrics: InternalMetricsConfig,
    #[serde(default)]
    api: ApiConfig,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawPipeline {
    Array(Vec<PipelineDef>),
    Single(PipelineConfig),
}

impl RawAppConfig {
    fn resolve(self) -> AppConfig {
        let pipelines = match self.pipeline {
            Some(RawPipeline::Array(defs)) => defs,
            Some(RawPipeline::Single(cfg)) => {
                let sources = self.capture.map(|c| c.sources).unwrap_or_default();
                vec![PipelineDef {
                    name: default_pipeline_name(),
                    dispatcher_count: cfg.dispatcher_count,
                    flow_shard_count: cfg.flow_shard_count,
                    queues: cfg.queues,
                    turn: cfg.turn,
                    metrics: cfg.metrics,
                    pcap_dump: cfg.pcap_dump,
                    sources,
                }]
            }
            None => {
                let sources = self.capture.map(|c| c.sources).unwrap_or_default();
                if sources.is_empty() {
                    Vec::new()
                } else {
                    vec![PipelineDef {
                        sources,
                        ..PipelineDef::default()
                    }]
                }
            }
        };
        let mut storage = self.storage;
        // Populate every known metrics granularity at load time so the loaded
        // `AppConfig` is the *effective* config: downstream consumers
        // (`/api/runtime-config`, retention sweep, logs) read a fully-merged
        // map, not a sparse user-overrides map. See [`resolve_metrics_retention`]
        // for the merge rule and unknown-label handling.
        let (resolved_metrics, unknowns) = resolve_metrics_retention(storage.retention.metrics);
        storage.retention.metrics = resolved_metrics;
        storage.retention.unknown_granularities = unknowns;
        AppConfig {
            pipelines,
            storage,
            internal_metrics: self.internal_metrics,
            api: self.api,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CaptureConfig {
    #[serde(default)]
    pub sources: Vec<CaptureSourceConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CaptureSourceConfig {
    Pcap {
        #[serde(default = "default_interface")]
        interface: String,
        #[serde(default)]
        bpf_filter: Option<String>,
        #[serde(default = "default_snaplen")]
        snaplen: u32,
        #[serde(default)]
        source_id: Option<String>,
    },
    PcapFile {
        path: String,
        #[serde(default)]
        realtime: bool,
        #[serde(default)]
        source_id: Option<String>,
    },
    CloudProbe {
        #[serde(default = "default_cloud_probe_endpoint")]
        endpoint: String,
        #[serde(default = "default_cloud_probe_hwm")]
        recv_hwm: i32,
    },
}

impl CaptureSourceConfig {
    /// Resolve the source_id for this source. Returns `Some` for static sources
    /// (pcap, pcap-file) with a default derived from interface/filename.
    /// Returns `None` for cloud-probe (source_id comes from batch UUID at runtime).
    pub fn resolved_source_id(&self) -> Option<String> {
        match self {
            Self::Pcap {
                source_id,
                interface,
                ..
            } => Some(source_id.clone().unwrap_or_else(|| interface.clone())),
            Self::PcapFile {
                source_id, path, ..
            } => {
                let base = std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path)
                    .to_string();
                Some(source_id.clone().unwrap_or(base))
            }
            Self::CloudProbe { .. } => None,
        }
    }
}

fn default_interface() -> String {
    "eth0".to_string()
}

fn default_snaplen() -> u32 {
    // Match libpcap/tcpdump's MAXIMUM_SNAPLEN. 65535 is not enough on Linux
    // interfaces with TSO/GSO/GRO/LRO offloads enabled (and especially `lo`),
    // where the kernel hands libpcap super-frames > 64 KB. Truncating those
    // strands LLM POST bodies and SSE responses mid-stream and breaks decode.
    262_144
}

fn default_cloud_probe_endpoint() -> String {
    "tcp://0.0.0.0:5555".to_string()
}

fn default_cloud_probe_hwm() -> i32 {
    1000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PipelineConfig {
    #[serde(default = "default_dispatcher_count")]
    pub dispatcher_count: usize,
    #[serde(default = "default_flow_shard_count")]
    pub flow_shard_count: usize,
    #[serde(default)]
    pub queues: QueueConfig,
    #[serde(default)]
    pub turn: TurnConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub pcap_dump: PcapDumpConfig,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            dispatcher_count: default_dispatcher_count(),
            flow_shard_count: default_flow_shard_count(),
            queues: QueueConfig::default(),
            turn: TurnConfig::default(),
            metrics: MetricsConfig::default(),
            pcap_dump: PcapDumpConfig::default(),
        }
    }
}

fn default_flow_shard_count() -> usize {
    4
}

/// Capacities of every bounded `mpsc` channel sitting between pipeline stages.
/// All default to 4096 — override individually under `[pipeline.queues]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueueConfig {
    /// capture → flow dispatcher
    #[serde(default = "default_queue_capacity")]
    pub raw: usize,
    /// flow dispatcher → each protocol parser shard (ParsedPacket)
    #[serde(default = "default_queue_capacity")]
    pub parsed_packet: usize,
    /// protocol parser → llm stage (HttpParseEvent, per shard)
    #[serde(default = "default_queue_capacity")]
    pub flow_event: usize,
    /// llm stage → each turn shard (per shard)
    #[serde(default = "default_queue_capacity")]
    pub turn_event: usize,
    /// llm stage → each metrics shard (per shard)
    #[serde(default = "default_queue_capacity")]
    pub metrics_event: usize,
    /// llm stage → storage sink (LlmCall records)
    #[serde(default = "default_queue_capacity")]
    pub call_sink: usize,
    /// turn stage → storage sink (AgentTurn records)
    #[serde(default = "default_queue_capacity")]
    pub turn_sink: usize,
    /// metrics stage → storage sink (LlmMetric records)
    #[serde(default = "default_queue_capacity")]
    pub metric_sink: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            raw: default_queue_capacity(),
            parsed_packet: default_queue_capacity(),
            flow_event: default_queue_capacity(),
            turn_event: default_queue_capacity(),
            metrics_event: default_queue_capacity(),
            call_sink: default_queue_capacity(),
            turn_sink: default_queue_capacity(),
            metric_sink: default_queue_capacity(),
        }
    }
}

fn default_queue_capacity() -> usize {
    4096
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub duckdb: DuckDbConfig,
    #[serde(default)]
    pub sink: StorageSinkConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            duckdb: DuckDbConfig::default(),
            sink: StorageSinkConfig::default(),
            retention: RetentionConfig::default(),
        }
    }
}

/// Default per-granularity retention for `llm_metrics`, in days.
///
/// The single source of truth for **known granularity labels** — the keys must
/// match the labels produced by `ts-metrics::aggregator::GRANULARITIES`. Used
/// at config-load time to populate any granularity the user did not override
/// and to drop typos like `"10sec"` with a warning. Adding a new granularity
/// requires updating ts-metrics + this single constant.
pub const DEFAULT_METRICS_RETENTION_DAYS: &[(&str, u32)] =
    &[("10s", 1), ("1m", 7), ("5m", 30), ("1h", 365)];

/// Merge user-supplied per-granularity retention overrides on top of
/// [`DEFAULT_METRICS_RETENTION_DAYS`]. Unknown labels (typos like `"10sec"`)
/// are dropped with a warn log so we don't silently keep junk in the loaded
/// config — by the time anything reads `RetentionConfig::metrics`, every key
/// is a known granularity and every known granularity has a value.
///
/// Returns the resolved map and the list of dropped unknown labels — the
/// latter is stashed on `RetentionConfig::unknown_granularities` so
/// `AppConfig::validate()` can surface them as `ConfigIssue`s.
pub fn resolve_metrics_retention(
    user: HashMap<String, u32>,
) -> (HashMap<String, u32>, Vec<String>) {
    let mut unknowns = Vec::new();
    for label in user.keys() {
        if !DEFAULT_METRICS_RETENTION_DAYS
            .iter()
            .any(|(known, _)| known == label)
        {
            tracing::warn!(
                granularity = label.as_str(),
                "retention: unknown metrics granularity in config; ignoring"
            );
            unknowns.push(label.clone());
        }
    }
    let resolved = DEFAULT_METRICS_RETENTION_DAYS
        .iter()
        .map(|(label, default_days)| {
            let days = user.get(*label).copied().unwrap_or(*default_days);
            ((*label).to_string(), days)
        })
        .collect();
    (resolved, unknowns)
}

/// Data retention policy for stored telemetry. Enabled by default with sane
/// per-table TTLs; set `enabled = false` or per-field `0` to opt out.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetentionConfig {
    #[serde(default = "default_retention_enabled")]
    pub enabled: bool,
    #[serde(default = "default_retention_check_interval_secs")]
    pub check_interval_secs: u64,
    /// Max age in days for `llm_calls`. `0` = never expire.
    #[serde(default = "default_calls_retention_days")]
    pub calls: u32,
    /// Max age in days for `agent_turns`. `0` = never expire.
    #[serde(default = "default_turns_retention_days")]
    pub turns: u32,
    /// Max age in days for `http_exchanges`. `0` = never expire. Raw headers +
    /// bodies make this the bulkiest table, so a short forensics window keeps
    /// storage bounded.
    #[serde(default = "default_http_exchanges_retention_days")]
    pub http_exchanges: u32,
    /// Per-granularity retention overrides for `llm_metrics`, in days. Key =
    /// granularity label (`"10s"`, `"1m"`, `"5m"`, `"1h"`). Missing keys fall
    /// back to defaults defined in `ts-storage::retention`; set a key to `0`
    /// to disable retention for that granularity.
    #[serde(default)]
    pub metrics: HashMap<String, u32>,
    /// Granularity labels in `metrics` that didn't match any known label
    /// (typo guard). Populated by [`resolve_metrics_retention`] at load time
    /// from the user's raw input — by the time you read this, the unknowns
    /// have already been dropped from `metrics`. Surfaced by
    /// `AppConfig::validate()` so `tokenscope config validate` can fail
    /// loudly on typos that the load-time warn easily missed.
    #[serde(skip)]
    pub unknown_granularities: Vec<String>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: default_retention_enabled(),
            check_interval_secs: default_retention_check_interval_secs(),
            calls: default_calls_retention_days(),
            turns: default_turns_retention_days(),
            http_exchanges: default_http_exchanges_retention_days(),
            metrics: HashMap::new(),
            unknown_granularities: Vec::new(),
        }
    }
}

fn default_retention_enabled() -> bool {
    true
}

fn default_calls_retention_days() -> u32 {
    30
}

fn default_turns_retention_days() -> u32 {
    // Must satisfy turns <= calls (see ConfigIssue::TurnsRetentionExceedsCalls).
    // Kept equal to calls so the default deploy is consistent without forcing
    // operators to think about the dependency.
    30
}

fn default_http_exchanges_retention_days() -> u32 {
    7
}

fn default_retention_check_interval_secs() -> u64 {
    3600
}

fn default_backend() -> String {
    "duckdb".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DuckDbConfig {
    #[serde(default = "default_duckdb_path")]
    pub path: String,
}

impl Default for DuckDbConfig {
    fn default() -> Self {
        Self {
            path: default_duckdb_path(),
        }
    }
}

fn default_duckdb_path() -> String {
    "data/tokenscope.duckdb".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InternalMetricsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
}

impl Default for InternalMetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_interval_secs(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_interval_secs() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            port: default_port(),
        }
    }
}

fn default_listen() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    3000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TurnConfig {
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    /// Buffer-and-finalize grace window: how long a buffered terminal call
    /// waits for fan-in jitter before its turn is partitioned and emitted.
    /// See `docs/design/04-turn.md` ("finalize_session").
    #[serde(default = "default_grace_ms")]
    pub grace_ms: u64,
    #[serde(default = "default_turn_shard_count")]
    pub shard_count: usize,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            sweep_interval_secs: default_sweep_interval_secs(),
            grace_ms: default_grace_ms(),
            shard_count: default_turn_shard_count(),
        }
    }
}

fn default_idle_timeout_secs() -> u64 {
    600
}

fn default_sweep_interval_secs() -> u64 {
    10
}

fn default_grace_ms() -> u64 {
    1000
}

fn default_turn_shard_count() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    #[serde(default = "default_metrics_shard_count")]
    pub shard_count: usize,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            shard_count: default_metrics_shard_count(),
        }
    }
}

fn default_metrics_shard_count() -> usize {
    1
}

/// Per-pipeline packet dump. When enabled, every non-heartbeat `RawPacket`
/// captured by this pipeline's sources is written to a Wireshark-openable
/// classic pcap file under
/// `<dir>/<pipeline_name>/<sanitized_source_id>/<minute>.pcap[.snappy]`.
/// The `<pipeline_name>` layer is appended automatically by the runtime —
/// multiple pipelines may safely share `dir` and stay fully isolated on
/// disk, including per-pipeline retention scope. Files rotate on
/// wall-clock minute boundaries (by packet timestamp); empty minutes are
/// skipped. Optional snappy framed compression appends `.snappy` to the
/// filename. Off by default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PcapDumpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_pcap_dump_dir")]
    pub dir: String,
    #[serde(default)]
    pub compression: PcapCompression,
    #[serde(default)]
    pub retention: PcapDumpRetentionConfig,
}

impl Default for PcapDumpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_pcap_dump_dir(),
            compression: PcapCompression::None,
            retention: PcapDumpRetentionConfig::default(),
        }
    }
}

fn default_pcap_dump_dir() -> String {
    "data/dumps".to_string()
}

/// File retention for `pcap_dump` output. Both rules default on so a
/// long-running deploy with `pcap_dump.enabled = true` cannot silently fill
/// the disk. Set `max_age_hours = 0` or `max_size_mb = 0` to disable that
/// individual rule; set `enabled = false` to skip retention entirely.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PcapDumpRetentionConfig {
    #[serde(default = "default_pcap_retention_enabled")]
    pub enabled: bool,
    #[serde(default = "default_pcap_retention_check_interval_secs")]
    pub check_interval_secs: u64,
    /// Delete files whose minute label is older than `now - max_age_hours`.
    /// `0` = no age cutoff.
    #[serde(default = "default_pcap_retention_max_age_hours")]
    pub max_age_hours: u32,
    /// Per-pipeline-dir total size cap in MiB. When the dump directory
    /// exceeds this, oldest minute files are deleted first until usage is
    /// back under the cap. `0` = no size cap.
    #[serde(default = "default_pcap_retention_max_size_mb")]
    pub max_size_mb: u64,
}

impl Default for PcapDumpRetentionConfig {
    fn default() -> Self {
        Self {
            enabled: default_pcap_retention_enabled(),
            check_interval_secs: default_pcap_retention_check_interval_secs(),
            max_age_hours: default_pcap_retention_max_age_hours(),
            max_size_mb: default_pcap_retention_max_size_mb(),
        }
    }
}

impl PcapDumpRetentionConfig {
    /// True when retention is enabled but every rule is `0` — the sweeper
    /// would have nothing to do. Mirrors `RetentionPolicy::is_empty` for
    /// storage retention so the same "exit-immediately" branch logic
    /// applies in `spawn_pcap_retention_task`.
    pub fn is_empty(&self) -> bool {
        self.max_age_hours == 0 && self.max_size_mb == 0
    }
}

fn default_pcap_retention_enabled() -> bool {
    true
}

fn default_pcap_retention_check_interval_secs() -> u64 {
    3600
}

fn default_pcap_retention_max_age_hours() -> u32 {
    24
}

fn default_pcap_retention_max_size_mb() -> u64 {
    10_240
}

/// Compression mode for pcap dump output. `None` writes plain `.pcap`;
/// `Snappy` writes snappy framed `.pcap.snappy` (decompress with `snzip
/// -d` or `snap::read::FrameDecoder` before opening in Wireshark).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PcapCompression {
    #[default]
    None,
    Snappy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageSinkConfig {
    #[serde(default = "default_sink_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_sink_flush_interval_ms")]
    pub flush_interval_ms: u64,
}

impl Default for StorageSinkConfig {
    fn default() -> Self {
        Self {
            batch_size: default_sink_batch_size(),
            flush_interval_ms: default_sink_flush_interval_ms(),
        }
    }
}

fn default_sink_batch_size() -> usize {
    1000
}

fn default_sink_flush_interval_ms() -> u64 {
    // 200 ms keeps the worst-case "row visible to a SELECT" under ~250 ms
    // when the producer is interval-bound (i.e. < 1000 calls/sec, the
    // typical wuneng-class workload). At 200 ms the writer fires ~5×
    // more often than at 1000 ms; each flush is ~5 ms (DuckDB appender),
    // so the extra wall-clock cost stays under 3 %.
    200
}

/// Severity of a [`ConfigIssue`]. `Error` blocks `tokenscope config validate`
/// (exit 1); `Warn` shows up in output but does not fail the command —
/// reserved for legal-but-suboptimal configurations (e.g. no pipelines,
/// which the runtime tolerates by serving the API in idle mode).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IssueSeverity {
    Warn,
    Error,
}

/// A semantic issue surfaced by [`AppConfig::validate`] beyond what TOML
/// parse and serde already catch. Stable JSON serialization (snake_case
/// `code`) so `tokenscope config validate` and `tokenscope doctor` produce
/// machine-readable output suitable for CI gates and AI agents.
///
/// Each variant has a fixed severity ([`ConfigIssue::severity`]) — variants
/// the runtime is documented to tolerate (no pipelines, no sources in a
/// pipeline) are `Warn`; everything else is `Error`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum ConfigIssue {
    /// No `[[pipeline]]` blocks (and no migrated single-pipeline) configured.
    /// Legal: the runtime serves the API in idle mode — useful when the
    /// user plans to attach via CLI flags (`-i`, `--pcap-file`) instead.
    NoPipelines,
    /// A pipeline was declared with zero `[[pipeline.sources]]`. Legal: the
    /// runtime tolerates it (same idle-API behavior as `NoPipelines`).
    NoSourcesInPipeline { pipeline: String },
    /// Two `[[pipeline]]` blocks share the same `name`.
    DuplicatePipelineName(String),
    /// Two static sources (pcap / pcap-file) resolved to the same `source_id`.
    /// `source_id` clashes break per-source dimensioning in metrics + storage.
    DuplicateSourceId { pipeline: String, source_id: String },
    /// Configured DuckDB path's parent directory is not writable by the
    /// current process. Only emitted when `storage.backend == "duckdb"`.
    StoragePathParentUnwritable { path: PathBuf },
    /// User supplied a granularity label under `[storage.retention.metrics]`
    /// that doesn't match any known granularity. Already dropped from the
    /// effective retention map at load time; reported here so typos fail
    /// validation rather than only producing a startup warn.
    UnknownRetentionGranularity(String),
    /// `[pipeline.pcap_dump.retention] enabled = true` but both
    /// `max_age_hours` and `max_size_mb` are `0`. Legal: the sweeper task
    /// exits immediately, so dumps accumulate unbounded — surfaced as a
    /// warning so operators don't think retention is running when nothing
    /// actually does.
    PcapDumpRetentionNoRules { pipeline: String },
    /// Pipeline has `pcap_dump.enabled = true` but its name doesn't
    /// produce a safe path component (empty after sanitization, or `.` /
    /// `..`). The runtime would silently disable pcap_dump for this
    /// pipeline since it can't build a valid `<dir>/<pipeline>/...` path.
    /// Fail validation hard so the operator sees the problem before deploy.
    UnsafePcapDumpPipelineName { pipeline: String },
    /// `agent_turns` retention outlives `llm_calls` retention, so the
    /// no-JOIN turn-detail read (`agent_turns.call_ids` → `llm_calls`
    /// IN-lookup) returns empty/partial calls for surviving turns once the
    /// calls sweep crosses their `request_time`. `turns_days = 0` is the
    /// sentinel for "never expire" (which always violates a finite
    /// `calls_days`); finite-vs-finite triggers when `turns_days > calls_days`.
    /// Only emitted when `calls_days > 0` — infinite calls retention can
    /// satisfy any turns retention.
    TurnsRetentionExceedsCalls { turns_days: u32, calls_days: u32 },
}

impl ConfigIssue {
    /// Severity of this issue — drives validate's exit code and doctor's
    /// `config.validate` status. The two `No*` variants are `Warn` because
    /// the runtime serves the API in idle mode when they apply; everything
    /// else is `Error`.
    pub fn severity(&self) -> IssueSeverity {
        match self {
            Self::NoPipelines
            | Self::NoSourcesInPipeline { .. }
            | Self::PcapDumpRetentionNoRules { .. } => IssueSeverity::Warn,
            Self::DuplicatePipelineName(_)
            | Self::DuplicateSourceId { .. }
            | Self::StoragePathParentUnwritable { .. }
            | Self::UnknownRetentionGranularity(_)
            | Self::UnsafePcapDumpPipelineName { .. }
            | Self::TurnsRetentionExceedsCalls { .. } => IssueSeverity::Error,
        }
    }
}

/// Wrapper that pairs a [`ConfigIssue`] with its [`IssueSeverity`] for JSON
/// output. Flattens the issue's adjacently-tagged `code`/`detail` fields so
/// the rendered shape is `{"severity": "warn", "code": "...", "detail": ...}`.
#[derive(Debug, Serialize)]
pub struct AnnotatedConfigIssue<'a> {
    pub severity: IssueSeverity,
    #[serde(flatten)]
    pub issue: &'a ConfigIssue,
}

impl<'a> From<&'a ConfigIssue> for AnnotatedConfigIssue<'a> {
    fn from(issue: &'a ConfigIssue) -> Self {
        Self {
            severity: issue.severity(),
            issue,
        }
    }
}

impl std::fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPipelines => write!(f, "no pipelines configured"),
            Self::NoSourcesInPipeline { pipeline } => {
                write!(f, "pipeline '{pipeline}' has no sources")
            }
            Self::DuplicatePipelineName(name) => {
                write!(f, "duplicate pipeline name: '{name}'")
            }
            Self::DuplicateSourceId {
                pipeline,
                source_id,
            } => write!(
                f,
                "duplicate source_id '{source_id}' in pipeline '{pipeline}'"
            ),
            Self::StoragePathParentUnwritable { path } => {
                write!(f, "storage path parent is not writable: {}", path.display())
            }
            Self::UnknownRetentionGranularity(label) => {
                let known: Vec<&str> = DEFAULT_METRICS_RETENTION_DAYS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect();
                write!(
                    f,
                    "unknown retention granularity '{label}' (expected one of: {})",
                    known.join(", ")
                )
            }
            Self::PcapDumpRetentionNoRules { pipeline } => write!(
                f,
                "pipeline '{pipeline}': pcap_dump.retention is enabled but \
                 max_age_hours and max_size_mb are both 0 (no rules to apply)"
            ),
            Self::UnsafePcapDumpPipelineName { pipeline } => write!(
                f,
                "pipeline '{pipeline}': pcap_dump.enabled = true but the \
                 pipeline name is not a safe path component (empty after \
                 sanitization, or '.' / '..'); the runtime cannot build a \
                 dump directory path"
            ),
            Self::TurnsRetentionExceedsCalls {
                turns_days,
                calls_days,
            } => {
                let turns_str = if *turns_days == 0 {
                    "never expire".to_string()
                } else {
                    format!("{turns_days}d")
                };
                write!(
                    f,
                    "storage.retention.turns ({turns_str}) outlives \
                     storage.retention.calls ({calls_days}d): turns whose \
                     llm_calls have been pruned will show empty/partial call \
                     lists. Set turns <= calls (or set calls = 0 for infinite)."
                )
            }
        }
    }
}

/// Best-effort writability probe for a directory. Walks up to the first
/// existing ancestor (so probing a path under a not-yet-created `data/`
/// directory still gives a meaningful answer about the cwd's writability),
/// then attempts to atomically create a uniquely-named probe file and
/// immediately remove it. The probe is the only reliable way to answer
/// "writable for this uid" across UNIX permission models — checking mode
/// bits via `metadata` misses ACLs, ownership, and effective uid.
///
/// Empty paths and parent-of-relative-paths that bottom out to "" are
/// normalized to `.` so a relative `data/foo.duckdb` whose `data/` doesn't
/// yet exist still probes the cwd (which is what `mkdir -p data` would do).
fn is_writable_dir(dir: &Path) -> bool {
    let mut probe_root = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir.to_path_buf()
    };
    while !probe_root.exists() {
        match probe_root.parent() {
            Some(p) => {
                if p.as_os_str().is_empty() {
                    probe_root = PathBuf::from(".");
                    break;
                }
                probe_root = p.to_path_buf();
            }
            None => return false,
        }
    }
    if !probe_root.is_dir() {
        return false;
    }
    let probe = probe_root.join(format!(".tokenscope_validate_probe.{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl AppConfig {
    /// Load configuration from a TOML file, with environment variable overrides.
    ///
    /// Environment variables are prefixed with `TS_` and use `__` as separator.
    /// For example: `TS_API__PORT=9090` overrides `api.port`.
    pub fn load(path: &Path) -> crate::error::Result<Self> {
        let config = Config::builder()
            .add_source(config::File::from(path))
            .add_source(
                config::Environment::with_prefix("TS")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()
            .map_err(AppError::from)?;

        let raw: RawAppConfig = config.try_deserialize().map_err(AppError::from)?;
        Ok(raw.resolve())
    }

    /// Run cross-field validation beyond what TOML parse + serde catches.
    /// Never panics; returns every issue found so callers can present a
    /// complete picture instead of failing on the first one.
    ///
    /// Callable safely after [`AppConfig::load`] succeeds. Used by
    /// `tokenscope config validate` and `tokenscope doctor`.
    pub fn validate(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        if self.pipelines.is_empty() {
            issues.push(ConfigIssue::NoPipelines);
        }

        let mut pipeline_names = std::collections::HashSet::new();
        let mut source_ids: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for def in &self.pipelines {
            if !pipeline_names.insert(def.name.clone()) {
                issues.push(ConfigIssue::DuplicatePipelineName(def.name.clone()));
            }
            if def.sources.is_empty() {
                issues.push(ConfigIssue::NoSourcesInPipeline {
                    pipeline: def.name.clone(),
                });
            }
            for source in &def.sources {
                if let Some(sid) = source.resolved_source_id() {
                    if source_ids.insert(sid.clone(), def.name.clone()).is_some() {
                        issues.push(ConfigIssue::DuplicateSourceId {
                            pipeline: def.name.clone(),
                            source_id: sid,
                        });
                    }
                }
            }
            if def.pcap_dump.enabled
                && def.pcap_dump.retention.enabled
                && def.pcap_dump.retention.is_empty()
            {
                issues.push(ConfigIssue::PcapDumpRetentionNoRules {
                    pipeline: def.name.clone(),
                });
            }
            if def.pcap_dump.enabled
                && !crate::path::is_safe_path_component(&def.name)
            {
                issues.push(ConfigIssue::UnsafePcapDumpPipelineName {
                    pipeline: def.name.clone(),
                });
            }
        }

        for unknown in &self.storage.retention.unknown_granularities {
            issues.push(ConfigIssue::UnknownRetentionGranularity(unknown.clone()));
        }

        // agent_turns references llm_calls via JSON call_ids; the no-JOIN
        // turn-detail read trusts that referenced calls still exist. If turns
        // outlive calls, surviving turns end up pointing at deleted call ids
        // and the detail view shows empty/partial results. 0 = never expire,
        // so finite calls_days with any larger (or 0) turns_days is broken.
        let calls_days = self.storage.retention.calls;
        let turns_days = self.storage.retention.turns;
        if calls_days > 0 && (turns_days == 0 || turns_days > calls_days) {
            issues.push(ConfigIssue::TurnsRetentionExceedsCalls {
                turns_days,
                calls_days,
            });
        }

        if self.storage.backend == "duckdb" {
            let path = Path::new(&self.storage.duckdb.path);
            let probe_dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            if !is_writable_dir(probe_dir) {
                issues.push(ConfigIssue::StoragePathParentUnwritable {
                    path: path.to_path_buf(),
                });
            }
        }

        issues
    }

    /// Parse a TOML string into an `AppConfig`. Useful for tests.
    #[cfg(test)]
    pub fn from_toml(s: &str) -> Self {
        let config = Config::builder()
            .add_source(config::File::from_str(s, config::FileFormat::Toml))
            .build()
            .expect("failed to build config from TOML string");
        let raw: RawAppConfig = config
            .try_deserialize()
            .expect("failed to deserialize RawAppConfig");
        raw.resolve()
    }
}

#[cfg(test)]
mod phase2_tests {
    use super::*;

    #[test]
    fn turn_config_has_shard_count_default_1() {
        let cfg = TurnConfig::default();
        assert_eq!(cfg.shard_count, 1);
    }

    #[test]
    fn metrics_config_has_shard_count_default_1() {
        let cfg = MetricsConfig::default();
        assert_eq!(cfg.shard_count, 1);
    }

    #[test]
    fn storage_sink_config_defaults() {
        let cfg = StorageSinkConfig::default();
        assert_eq!(cfg.batch_size, 1000);
        // 200 ms — see comment on default_sink_flush_interval_ms for rationale.
        assert_eq!(cfg.flush_interval_ms, 200);
    }

    #[test]
    fn retention_config_enabled_by_default_with_sane_ttls() {
        let cfg = RetentionConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.check_interval_secs, 3600);
        assert_eq!(cfg.calls, 30);
        // turns must not exceed calls — see ConfigIssue::TurnsRetentionExceedsCalls.
        assert_eq!(cfg.turns, 30);
        assert_eq!(cfg.http_exchanges, 7);
        // metrics map stays empty — per-granularity defaults are merged at
        // policy-build time in ts-storage so users can override one label
        // without dropping the rest.
        assert!(cfg.metrics.is_empty());
    }

    #[test]
    fn storage_config_embeds_retention_defaults() {
        let cfg = StorageConfig::default();
        assert!(cfg.retention.enabled);
        assert_eq!(cfg.retention.calls, 30);
        // turns must not exceed calls — see ConfigIssue::TurnsRetentionExceedsCalls.
        assert_eq!(cfg.retention.turns, 30);
    }

    #[test]
    fn retention_config_parses_per_granularity_toml() {
        let toml = r#"
            enabled = true
            check_interval_secs = 60
            calls = 7
            turns = 30
            [metrics]
            "10s" = 1
            "1m" = 7
            "1h" = 365
        "#;
        let cfg: RetentionConfig = Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .expect("build config")
            .try_deserialize()
            .expect("deserialize retention");
        assert!(cfg.enabled);
        assert_eq!(cfg.check_interval_secs, 60);
        assert_eq!(cfg.calls, 7);
        assert_eq!(cfg.turns, 30);
        assert_eq!(cfg.metrics.get("10s"), Some(&1));
        assert_eq!(cfg.metrics.get("1m"), Some(&7));
        assert_eq!(cfg.metrics.get("1h"), Some(&365));
        assert_eq!(cfg.metrics.get("5m"), None);
    }

    #[test]
    fn pcap_config_with_custom_source_id() {
        let toml = r#"
            [[capture.sources]]
            type = "pcap"
            interface = "eth0"
            source_id = "my-source"
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert_eq!(cfg.pipelines.len(), 1);
        match &cfg.pipelines[0].sources[0] {
            CaptureSourceConfig::Pcap { source_id, .. } => {
                assert_eq!(source_id.as_deref(), Some("my-source"));
            }
            _ => panic!("expected Pcap"),
        }
    }

    #[test]
    fn resolved_source_id_defaults() {
        let pcap = CaptureSourceConfig::Pcap {
            interface: "eth1".to_string(),
            bpf_filter: None,
            snaplen: 65535,
            source_id: None,
        };
        assert_eq!(pcap.resolved_source_id(), Some("eth1".to_string()));

        let pcap_file = CaptureSourceConfig::PcapFile {
            path: "/data/captures/test.pcap".to_string(),
            realtime: false,
            source_id: None,
        };
        assert_eq!(pcap_file.resolved_source_id(), Some("test".to_string()));

        let cloud = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
            recv_hwm: 1000,
        };
        assert_eq!(cloud.resolved_source_id(), None);
    }

    #[test]
    fn queue_config_defaults_all_4096() {
        let cfg = QueueConfig::default();
        assert_eq!(cfg.raw, 4096);
        assert_eq!(cfg.parsed_packet, 4096);
        assert_eq!(cfg.flow_event, 4096);
        assert_eq!(cfg.turn_event, 4096);
        assert_eq!(cfg.metrics_event, 4096);
        assert_eq!(cfg.call_sink, 4096);
        assert_eq!(cfg.turn_sink, 4096);
        assert_eq!(cfg.metric_sink, 4096);
    }

    #[test]
    fn pipeline_array_with_nested_sources() {
        let toml = r#"
            [[pipeline]]
            name = "gpu-cluster"
            flow_shard_count = 8

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth1"

            [[pipeline]]
            name = "cpu-pool"

            [[pipeline.sources]]
            type = "pcap-file"
            path = "/data/cpu.pcap"
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert_eq!(cfg.pipelines.len(), 2);

        assert_eq!(cfg.pipelines[0].name, "gpu-cluster");
        assert_eq!(cfg.pipelines[0].flow_shard_count, 8);
        assert_eq!(cfg.pipelines[0].sources.len(), 2);

        assert_eq!(cfg.pipelines[1].name, "cpu-pool");
        assert_eq!(cfg.pipelines[1].flow_shard_count, 4); // default
        assert_eq!(cfg.pipelines[1].sources.len(), 1);
    }

    #[test]
    fn old_format_migrates_to_single_pipeline() {
        let toml = r#"
            [pipeline]
            flow_shard_count = 2

            [[capture.sources]]
            type = "pcap"
            interface = "lo0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert_eq!(cfg.pipelines.len(), 1);
        assert_eq!(cfg.pipelines[0].name, "default");
        assert_eq!(cfg.pipelines[0].flow_shard_count, 2);
        assert_eq!(cfg.pipelines[0].sources.len(), 1);
        match &cfg.pipelines[0].sources[0] {
            CaptureSourceConfig::Pcap { interface, .. } => {
                assert_eq!(interface, "lo0");
            }
            _ => panic!("expected Pcap"),
        }
    }

    #[test]
    fn empty_config_yields_no_pipelines() {
        let cfg = AppConfig::from_toml("");
        assert!(cfg.pipelines.is_empty());
    }

    #[test]
    fn pcap_dump_disabled_by_default() {
        let toml = r#"
            [[pipeline]]
            name = "p"

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert!(!cfg.pipelines[0].pcap_dump.enabled);
        assert_eq!(cfg.pipelines[0].pcap_dump.dir, "data/dumps");
        assert_eq!(
            cfg.pipelines[0].pcap_dump.compression,
            crate::config::PcapCompression::None,
        );
    }

    #[test]
    fn load_populates_missing_metrics_granularities_with_defaults() {
        // Empty user override → loaded config carries every known granularity
        // at its default day-count, so downstream consumers (API surface,
        // retention sweeper) never have to merge again.
        let cfg = AppConfig::from_toml("");
        let m = &cfg.storage.retention.metrics;
        assert_eq!(m.len(), DEFAULT_METRICS_RETENTION_DAYS.len());
        for (label, days) in DEFAULT_METRICS_RETENTION_DAYS {
            assert_eq!(m.get(*label), Some(days), "missing {label}");
        }
    }

    #[test]
    fn load_user_override_for_one_granularity_keeps_other_defaults() {
        // The whole reason for default-merge: overriding "1h" must not silently
        // drop retention for the other three labels.
        let toml = r#"
            [storage.retention.metrics]
            "1h" = 730
        "#;
        let cfg = AppConfig::from_toml(toml);
        let m = &cfg.storage.retention.metrics;
        assert_eq!(m.get("10s"), Some(&1));
        assert_eq!(m.get("1m"), Some(&7));
        assert_eq!(m.get("5m"), Some(&30));
        assert_eq!(m.get("1h"), Some(&730));
    }

    #[test]
    fn load_drops_unknown_metrics_granularity() {
        // Typos like "10sec" must not survive into the loaded config; they'd
        // either silently retain forever (not in the iteration) or worse,
        // ship to the API surface and confuse the operator.
        let toml = r#"
            [storage.retention.metrics]
            "10sec" = 1
            "1m" = 7
        "#;
        let cfg = AppConfig::from_toml(toml);
        let m = &cfg.storage.retention.metrics;
        assert!(m.get("10sec").is_none());
        assert_eq!(m.get("1m"), Some(&7));
    }

    #[test]
    fn validate_empty_config_reports_no_pipelines() {
        let cfg = AppConfig::from_toml("");
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| matches!(i, ConfigIssue::NoPipelines)),
            "expected NoPipelines, got {issues:?}"
        );
    }

    #[test]
    fn validate_pipeline_without_sources_is_an_issue() {
        let toml = r#"
            [[pipeline]]
            name = "empty"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::NoSourcesInPipeline { pipeline } if pipeline == "empty")),
            "expected NoSourcesInPipeline('empty'), got {issues:?}"
        );
    }

    #[test]
    fn validate_duplicate_pipeline_names() {
        let toml = r#"
            [[pipeline]]
            name = "dup"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [[pipeline]]
            name = "dup"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth1"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::DuplicatePipelineName(n) if n == "dup")),
            "expected DuplicatePipelineName('dup'), got {issues:?}"
        );
    }

    #[test]
    fn validate_duplicate_source_ids_across_pipelines() {
        // Two pipelines, both with a pcap source on the same interface →
        // resolved_source_id collides.
        let toml = r#"
            [[pipeline]]
            name = "a"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [[pipeline]]
            name = "b"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::DuplicateSourceId { source_id, .. } if source_id == "eth0")),
            "expected DuplicateSourceId('eth0'), got {issues:?}"
        );
    }

    #[test]
    fn validate_unknown_retention_granularity_surfaces_after_load() {
        // Typos are dropped from the effective config but stashed on
        // `unknown_granularities` so `validate()` can still flag them.
        let toml = r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage.retention.metrics]
            "10sec" = 1
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::UnknownRetentionGranularity(g) if g == "10sec")),
            "expected UnknownRetentionGranularity('10sec'), got {issues:?}"
        );
    }

    #[test]
    fn validate_storage_path_parent_unwritable() {
        // A path under a definitely-not-writable root surfaces the issue.
        // `/proc/tokenscope-validate-test` exists on Linux but is not writable;
        // on macOS we use `/dev/null/` which is unwritable as a directory.
        let toml = r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage.duckdb]
            path = "/dev/null/cant-write/here.duckdb"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::StoragePathParentUnwritable { .. })),
            "expected StoragePathParentUnwritable, got {issues:?}"
        );
    }

    #[test]
    fn validate_turns_retention_finite_exceeds_calls_is_error() {
        // turns 30d > calls 7d → turns linger after their child llm_calls
        // are pruned; the no-JOIN turn-detail read returns empty/partial calls.
        let toml = r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage.retention]
            calls = 7
            turns = 30
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| matches!(
                i,
                ConfigIssue::TurnsRetentionExceedsCalls {
                    turns_days: 30,
                    calls_days: 7
                }
            )),
            "expected TurnsRetentionExceedsCalls(30, 7), got {issues:?}"
        );
    }

    #[test]
    fn validate_turns_infinite_with_calls_finite_is_error() {
        // turns = 0 (never expire) and calls > 0 → turns outlive every call.
        // Detected as the same issue (sentinel turns_days = 0).
        let toml = r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage.retention]
            calls = 7
            turns = 0
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| matches!(
                i,
                ConfigIssue::TurnsRetentionExceedsCalls {
                    turns_days: 0,
                    calls_days: 7
                }
            )),
            "expected TurnsRetentionExceedsCalls(0, 7), got {issues:?}"
        );
    }

    #[test]
    fn validate_turns_finite_with_calls_infinite_is_ok() {
        // calls = 0 (infinite) trivially outlives any finite turns retention.
        let toml = r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage.retention]
            calls = 0
            turns = 30
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::TurnsRetentionExceedsCalls { .. })),
            "expected no TurnsRetentionExceedsCalls, got {issues:?}"
        );
    }

    #[test]
    fn validate_turns_equal_calls_is_ok() {
        let toml = r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage.retention]
            calls = 7
            turns = 7
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::TurnsRetentionExceedsCalls { .. })),
            "expected no TurnsRetentionExceedsCalls, got {issues:?}"
        );
    }

    #[test]
    fn validate_default_retention_does_not_violate_constraint() {
        // Defaults must pass the turns<=calls rule out of the box; otherwise
        // every default deploy is broken before the operator touches config.
        let toml = r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::TurnsRetentionExceedsCalls { .. })),
            "default config raised retention constraint: {issues:?}"
        );
    }

    #[test]
    fn validate_clean_config_has_no_issues() {
        let tmp = std::env::temp_dir();
        let toml = format!(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage.duckdb]
            path = "{}/tokenscope-validate-clean.duckdb"
            "#,
            tmp.display()
        );
        let cfg = AppConfig::from_toml(&toml);
        let issues = cfg.validate();
        assert!(issues.is_empty(), "expected no issues, got {issues:?}");
    }

    #[test]
    fn config_issue_serializes_with_snake_case_code() {
        let issue = ConfigIssue::DuplicatePipelineName("foo".to_string());
        let v = serde_json::to_value(&issue).unwrap();
        assert_eq!(v["code"], "duplicate_pipeline_name");
        assert_eq!(v["detail"], "foo");

        let unit = ConfigIssue::NoPipelines;
        let v = serde_json::to_value(&unit).unwrap();
        assert_eq!(v["code"], "no_pipelines");
    }

    #[test]
    fn no_pipelines_and_no_sources_are_warnings() {
        // The runtime serves the API in idle mode for both — keep them
        // visible (so AI agents see the intent gap) but don't fail validate.
        assert_eq!(ConfigIssue::NoPipelines.severity(), IssueSeverity::Warn);
        assert_eq!(
            ConfigIssue::NoSourcesInPipeline {
                pipeline: "p".to_string()
            }
            .severity(),
            IssueSeverity::Warn
        );
    }

    #[test]
    fn breaking_misconfigurations_are_errors() {
        assert_eq!(
            ConfigIssue::DuplicatePipelineName("d".to_string()).severity(),
            IssueSeverity::Error
        );
        assert_eq!(
            ConfigIssue::DuplicateSourceId {
                pipeline: "p".to_string(),
                source_id: "s".to_string()
            }
            .severity(),
            IssueSeverity::Error
        );
        assert_eq!(
            ConfigIssue::StoragePathParentUnwritable {
                path: PathBuf::from("/no")
            }
            .severity(),
            IssueSeverity::Error
        );
        assert_eq!(
            ConfigIssue::UnknownRetentionGranularity("10sec".to_string()).severity(),
            IssueSeverity::Error
        );
    }

    #[test]
    fn annotated_issue_includes_severity_in_json() {
        let issue = ConfigIssue::NoPipelines;
        let annotated = AnnotatedConfigIssue::from(&issue);
        let v = serde_json::to_value(&annotated).unwrap();
        assert_eq!(v["severity"], "warn");
        assert_eq!(v["code"], "no_pipelines");

        let issue = ConfigIssue::DuplicatePipelineName("d".to_string());
        let annotated = AnnotatedConfigIssue::from(&issue);
        let v = serde_json::to_value(&annotated).unwrap();
        assert_eq!(v["severity"], "error");
        assert_eq!(v["code"], "duplicate_pipeline_name");
        assert_eq!(v["detail"], "d");
    }

    #[test]
    fn pcap_dump_parses_full_block() {
        let toml = r#"
            [[pipeline]]
            name = "p"

            [pipeline.pcap_dump]
            enabled = true
            dir = "/tmp/dumps"
            compression = "snappy"

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let d = &cfg.pipelines[0].pcap_dump;
        assert!(d.enabled);
        assert_eq!(d.dir, "/tmp/dumps");
        assert_eq!(d.compression, crate::config::PcapCompression::Snappy);
    }

    #[test]
    fn pcap_dump_retention_has_aggressive_defaults() {
        // No explicit retention block — defaults must keep both rules on so
        // a long-running deploy with `pcap_dump.enabled` cannot silently
        // fill the disk. 24-hour TTL covers typical post-mortem windows;
        // 10 GiB size cap is the disk safety net.
        let toml = r#"
            [[pipeline]]
            name = "p"

            [pipeline.pcap_dump]
            enabled = true

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let r = &cfg.pipelines[0].pcap_dump.retention;
        assert!(r.enabled);
        assert_eq!(r.check_interval_secs, 3600);
        assert_eq!(r.max_age_hours, 24);
        assert_eq!(r.max_size_mb, 10_240);
    }

    #[test]
    fn pcap_dump_retention_parses_full_block() {
        let toml = r#"
            [[pipeline]]
            name = "p"

            [pipeline.pcap_dump]
            enabled = true

            [pipeline.pcap_dump.retention]
            enabled = false
            check_interval_secs = 60
            max_age_hours = 24
            max_size_mb = 0

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let r = &cfg.pipelines[0].pcap_dump.retention;
        assert!(!r.enabled);
        assert_eq!(r.check_interval_secs, 60);
        assert_eq!(r.max_age_hours, 24);
        assert_eq!(r.max_size_mb, 0);
    }

    #[test]
    fn validate_pcap_dump_retention_enabled_but_no_rules() {
        // Both rules zeroed while retention is enabled → the sweeper would
        // exit immediately. Surface as a warning so operators don't think
        // their dumps are being cleaned when nothing actually runs.
        let toml = r#"
            [[pipeline]]
            name = "p"

            [pipeline.pcap_dump]
            enabled = true

            [pipeline.pcap_dump.retention]
            enabled = true
            max_age_hours = 0
            max_size_mb = 0

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| matches!(
                i,
                ConfigIssue::PcapDumpRetentionNoRules { pipeline } if pipeline == "p"
            )),
            "expected PcapDumpRetentionNoRules('p'), got {issues:?}"
        );
        // Severity is `warn` — runtime tolerates (task simply exits).
        let issue = issues
            .iter()
            .find(|i| matches!(i, ConfigIssue::PcapDumpRetentionNoRules { .. }))
            .unwrap();
        assert_eq!(issue.severity(), IssueSeverity::Warn);
    }

    #[test]
    fn validate_pcap_dump_retention_disabled_does_not_warn() {
        // Retention disabled — empty rules are irrelevant; no issue should fire.
        let toml = r#"
            [[pipeline]]
            name = "p"

            [pipeline.pcap_dump]
            enabled = true

            [pipeline.pcap_dump.retention]
            enabled = false
            max_age_hours = 0
            max_size_mb = 0

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::PcapDumpRetentionNoRules { .. })),
            "did not expect PcapDumpRetentionNoRules, got {issues:?}"
        );
    }

    #[test]
    fn validate_pipelines_sharing_pcap_dump_dir_is_allowed() {
        // The runtime auto-appends a sanitized pipeline-name layer
        // (`<dir>/<pipeline>/<source_id>/`), so two pipelines sharing the
        // configured `pcap_dump.dir` end up with disjoint effective
        // directories — no validation issue should fire. `DuplicatePipelineName`
        // already prevents the only collision case (same name + same base).
        let toml = r#"
            [[pipeline]]
            name = "a"
            [pipeline.pcap_dump]
            enabled = true
            dir = "/tmp/dumps-shared"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [[pipeline]]
            name = "b"
            [pipeline.pcap_dump]
            enabled = true
            dir = "/tmp/dumps-shared"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth1"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        // No issue should be raised for the shared dir or the source_ids
        // (eth0 vs eth1 don't collide). Other unrelated issues — e.g.
        // `StoragePathParentUnwritable` from the default duckdb path under
        // a non-writable cwd in some test runners — are out of scope.
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::DuplicateSourceId { .. })),
            "expected no DuplicateSourceId, got {issues:?}"
        );
    }

    #[test]
    fn validate_unsafe_pipeline_name_with_pcap_dump_enabled_is_an_error() {
        // Pipeline name sanitizes to empty/./.. → the runtime would
        // silently disable pcap_dump for this pipeline. Surface as a
        // hard error so `tokenscope config validate` catches it before
        // deploy. We test '..' specifically; other unsafe shapes share
        // the same code path (covered by ts-common::path tests).
        let toml = r#"
            [[pipeline]]
            name = ".."

            [pipeline.pcap_dump]
            enabled = true

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| matches!(
                i,
                ConfigIssue::UnsafePcapDumpPipelineName { pipeline } if pipeline == ".."
            )),
            "expected UnsafePcapDumpPipelineName('..'), got {issues:?}"
        );
        let issue = issues
            .iter()
            .find(|i| matches!(i, ConfigIssue::UnsafePcapDumpPipelineName { .. }))
            .unwrap();
        assert_eq!(issue.severity(), IssueSeverity::Error);
    }

    #[test]
    fn validate_unsafe_pipeline_name_with_pcap_dump_disabled_is_silent() {
        // Same name, but pcap_dump is off — the runtime never tries to
        // build a path from this name, so no issue should fire.
        let toml = r#"
            [[pipeline]]
            name = ".."

            [pipeline.pcap_dump]
            enabled = false

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::UnsafePcapDumpPipelineName { .. })),
            "did not expect UnsafePcapDumpPipelineName when dump disabled, got {issues:?}"
        );
    }

    #[test]
    fn validate_pcap_dump_disabled_skips_retention_check() {
        // pcap_dump itself is off — retention config is irrelevant.
        let toml = r#"
            [[pipeline]]
            name = "p"

            [pipeline.pcap_dump]
            enabled = false

            [pipeline.pcap_dump.retention]
            enabled = true
            max_age_hours = 0
            max_size_mb = 0

            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"
        "#;
        let cfg = AppConfig::from_toml(toml);
        let issues = cfg.validate();
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, ConfigIssue::PcapDumpRetentionNoRules { .. })),
            "did not expect PcapDumpRetentionNoRules when dump disabled, got {issues:?}"
        );
    }
}
