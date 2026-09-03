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
/// 2. `$XDG_CONFIG_HOME/heron/config.toml` — user override (XDG-aware).
/// 3. `~/.config/heron/config.toml` — user override (XDG default).
/// 4. `/etc/heron/config.toml` — system-wide install (dropped by
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
            paths.push(PathBuf::from(xdg).join("heron/config.toml"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            paths.push(PathBuf::from(home).join(".config/heron/config.toml"));
        }
    }
    paths.push(PathBuf::from("/etc/heron/config.toml"));

    paths
}

/// Walk [`config_search_paths`] and return the first path that exists as a
/// regular file. Returns `None` when no config is found anywhere — callers
/// should print the searched paths so the user knows what to fix.
pub fn discover_config_path() -> Option<PathBuf> {
    config_search_paths().into_iter().find(|p| p.is_file())
}

/// TOML representation of `ClassifierConfig`. Each field is a plain `Vec<String>` so
/// TOML arrays work naturally. Empty Vec means "use the built-in default for that
/// field" — see `ClassifierConfig::from_toml` in `h-llm`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClassifierConfigToml {
    #[serde(default)]
    pub cli_tool_allowlist: Vec<String>,
    #[serde(default)]
    pub orchestrator_tool_names: Vec<String>,
    #[serde(default)]
    pub mcp_tool_prefixes: Vec<String>,
    #[serde(default)]
    pub known_tool_registry: Vec<String>,
}

/// Policy for bounding the size of stored HTTP bodies.
///
/// Heron stores each call's request and response body for display and
/// re-classification. With 2026-era 1M-token contexts a single body can be
/// many megabytes; the storage write buffer batches ~1000 rows, so holding
/// full bodies is a real memory-pressure / stability risk on capture nodes.
///
/// This policy keeps the first `head_bytes` and last `tail_bytes` of each
/// stored body (eliding the middle), so the head (model / params / system /
/// tools / first messages) and the tail (final messages + the trailing
/// `usage` block on non-streaming responses) are always retained while total
/// stored bytes are bounded. `LlmCall.body_bytes_dropped` records how many
/// bytes were elided.
///
/// The cap applies **only to what is stored** — usage / model / agent
/// classification are always extracted from the full body upstream, so the
/// cap never reduces metric accuracy. It is **symmetric**: the same head/tail
/// budget applies to request and response bodies. The struct is shaped so a
/// separate response budget can be added later without breaking configs.
/// A body that fits within `head_bytes + tail_bytes` is stored verbatim.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct BodyCapConfig {
    /// Master switch. When `false`, bodies are stored verbatim (legacy
    /// behavior, unbounded).
    pub enabled: bool,
    /// Bytes retained from the start of each stored body.
    pub head_bytes: usize,
    /// Bytes retained from the end of each stored body. Covers the trailing
    /// `usage` block on non-streaming responses.
    pub tail_bytes: usize,
}

impl Default for BodyCapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            head_bytes: 256 * 1024, // 256 KiB
            tail_bytes: 64 * 1024,  // 64 KiB
        }
    }
}

impl BodyCapConfig {
    /// Total bytes retained per body when the cap is active. A body at or
    /// below this size is stored verbatim (no bytes dropped).
    pub fn budget(&self) -> usize {
        self.head_bytes.saturating_add(self.tail_bytes)
    }
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
    #[serde(default)]
    pub agent_classifier: ClassifierConfigToml,
    /// Stored-body size cap. See [`BodyCapConfig`].
    pub body_cap: BodyCapConfig,
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
    #[serde(default)]
    agent_classifier: ClassifierConfigToml,
    #[serde(default)]
    body_cap: BodyCapConfig,
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
        // The backend formerly known as sglog/sglake is Aglake as of its 0.3
        // release. Normalize the old value here so exactly one spelling reaches
        // the runtime — every `backend == "aglake"` check downstream stays a
        // single comparison — and remember that we did, so `validate()` can say
        // so instead of silently accepting a name that no longer exists.
        if storage.backend == LEGACY_AGLAKE_BACKEND {
            storage.legacy_backend_name = Some(storage.backend.clone());
            storage.backend = AGLAKE_BACKEND.to_string();
        }
        AppConfig {
            pipelines,
            storage,
            internal_metrics: self.internal_metrics,
            api: self.api,
            agent_classifier: self.agent_classifier,
            body_cap: self.body_cap,
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
        /// Replay the file this many times back-to-back (default 1 = single
        /// pass, today's behavior). Each pass is tagged with a fresh
        /// per-iteration source_id so its flows are distinct (not deduped as
        /// retransmits) — this is what lets the load soak drive sustained
        /// full-pipeline + storage traffic from one corpus. Ignored when
        /// `loop_secs` > 0.
        #[serde(default = "default_loop_count")]
        loop_count: u32,
        /// Replay in a loop until this many seconds have elapsed (0 = disabled
        /// → use `loop_count`). Takes precedence over `loop_count`. Used by the
        /// duration-bounded load/longevity soak.
        #[serde(default)]
        loop_secs: u64,
        /// Pace emission to this many packets/sec (0 = unthrottled = today's
        /// behavior). Lets the load soak drive a steady, prod-like rate from a
        /// looped corpus instead of an as-fast-as-possible firehose that just
        /// saturates the channels. Applies across all passes.
        #[serde(default)]
        rate_pps: u32,
    },
    CloudProbe {
        #[serde(default = "default_cloud_probe_endpoint")]
        endpoint: String,
        #[serde(default = "default_cloud_probe_hwm")]
        recv_hwm: i32,
    },
    /// eBPF SSL-uprobe capture (Linux only). Attaches uprobes to `SSL_read` /
    /// `SSL_write` to observe the plaintext of TLS-encrypted LLM API calls
    /// without a proxy, then reconstructs synthetic TCP frames that feed the
    /// existing pipeline. Construction is Linux-gated in the capture factory;
    /// the config itself parses on every platform so a shared config file is
    /// portable.
    Ebpf {
        #[serde(default)]
        source_id: Option<String>,
        /// Explicit `libssl` paths to attach to. Empty = autodetect by scanning
        /// running processes' mapped libraries (`/proc/*/maps`).
        #[serde(default)]
        ssl_libs: Vec<String>,
        /// Static-binary targets (Phase 3): binaries with no dynamic `libssl`
        /// whose TLS functions are located by symbol or byte-pattern offset.
        #[serde(default)]
        targets: Vec<EbpfTarget>,
        /// Restrict capture to these PIDs. Empty = all processes.
        #[serde(default)]
        pid_allowlist: Vec<u32>,
        /// Target payload size (bytes) per synthesized TCP segment.
        #[serde(default = "default_ebpf_segment_size")]
        segment_size: u32,
    },
}

/// A static-binary uprobe target for eBPF capture (Phase 3). Used for runtimes
/// that statically link their TLS library (e.g. Claude Code's Bun binary), so
/// there is no `libssl.so` to attach to by name.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EbpfTarget {
    /// Absolute path to the executable to attach uprobes to.
    pub binary: String,
    /// TLS library flavor baked into the binary, selecting the symbol /
    /// byte-pattern strategy used to locate `SSL_read` / `SSL_write`.
    /// e.g. `"openssl"`, `"boringssl"`.
    #[serde(default = "default_ebpf_target_flavor")]
    pub flavor: String,
    /// Optional byte-signature override for `SSL_write`'s prologue, as a pattern
    /// of space-separated hex bytes with `??` wildcards
    /// (e.g. `"55 41 57 ?? 48 8b"`). Signatures are version-specific to one
    /// statically-linked TLS build, so they live in config (data) rather than
    /// code: an operator can pin their exact Bun / Claude Code release without a
    /// rebuild. When unset, the loader falls back to the built-in signature for
    /// `flavor` (if any).
    #[serde(default)]
    pub write_sig: Option<String>,
    /// Optional byte-signature override for `SSL_read`'s prologue. See
    /// [`Self::write_sig`].
    #[serde(default)]
    pub read_sig: Option<String>,
    /// Explicit `SSL_write` file offset, bypassing signature scanning entirely.
    /// For when the offset is already known (e.g. from `sigscan_probe` or RE) —
    /// also the validation path used while deriving a signature for a new build.
    #[serde(default)]
    pub write_offset: Option<u64>,
    /// Explicit `SSL_read` file offset. See [`Self::write_offset`].
    #[serde(default)]
    pub read_offset: Option<u64>,
}

fn default_ebpf_target_flavor() -> String {
    "boringssl".to_string()
}

fn default_ebpf_segment_size() -> u32 {
    // 16 KiB — matches h-capture's synth DEFAULT_SEGMENT_SIZE. Kept in sync by
    // value (h-common must not depend on h-capture).
    16 * 1024
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
            Self::Ebpf { source_id, .. } => {
                Some(source_id.clone().unwrap_or_else(|| "ebpf".to_string()))
            }
        }
    }
}

fn default_interface() -> String {
    "eth0".to_string()
}

fn default_loop_count() -> u32 {
    1
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
///
/// Field names mirror the queue probe metrics surfaced by `internal_metrics`
/// (and the `pipeline-health` UI), modulo the `q_` prefix: e.g. config
/// `agent_calls` ↔ metric `q_agent_calls`. The `storage_*` quartet feeds the
/// shared sink that fans every pipeline into one DB writer.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueueConfig {
    /// capture → flow dispatcher
    #[serde(default = "default_queue_capacity")]
    pub raw_pkts: usize,
    /// flow dispatcher → each protocol parser shard (ParsedPacket)
    #[serde(default = "default_queue_capacity")]
    pub parsed_pkts: usize,
    /// protocol parser → http joiner (HttpParseEvent, per shard)
    #[serde(default = "default_queue_capacity")]
    pub http_parse_events: usize,
    /// http joiner → llm stage (HttpJoinerEvent, per shard)
    #[serde(default = "default_queue_capacity")]
    pub http_joiner_events: usize,
    /// llm stage → each turn shard (AgentCall, per shard)
    #[serde(default = "default_queue_capacity")]
    pub agent_calls: usize,
    /// llm stage → each metrics shard (LlmEvent, per shard)
    #[serde(default = "default_queue_capacity")]
    pub llm_events: usize,
    /// llm stage → shared storage sink (LlmCall records)
    #[serde(default = "default_queue_capacity")]
    pub storage_calls: usize,
    /// turn stage → shared storage sink (Trace records)
    #[serde(default = "default_queue_capacity")]
    pub storage_turns: usize,
    /// metrics stage → shared storage sink (LlmMetric records)
    #[serde(default = "default_queue_capacity")]
    pub storage_metrics: usize,
    /// http joiner → shared storage sink (HttpExchange records)
    #[serde(default = "default_queue_capacity")]
    pub storage_exchanges: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            raw_pkts: default_queue_capacity(),
            parsed_pkts: default_queue_capacity(),
            http_parse_events: default_queue_capacity(),
            http_joiner_events: default_queue_capacity(),
            agent_calls: default_queue_capacity(),
            llm_events: default_queue_capacity(),
            storage_calls: default_queue_capacity(),
            storage_turns: default_queue_capacity(),
            storage_metrics: default_queue_capacity(),
            storage_exchanges: default_queue_capacity(),
        }
    }
}

fn default_queue_capacity() -> usize {
    4096
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// Which backend to run. The legacy value `"sglake"` is normalized to
    /// `"aglake"` at load time — see [`StorageConfig::legacy_backend_name`].
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub duckdb: DuckDbConfig,
    #[serde(default)]
    pub clickhouse: ClickHouseConfig,
    /// The legacy `[storage.sglake]` table is still accepted via serde alias,
    /// which also covers the `TS_STORAGE__SGLAKE__*` environment overrides.
    #[serde(default, alias = "sglake")]
    pub aglake: AglakeConfig,
    #[serde(default)]
    pub sink: StorageSinkConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Set when `backend` arrived as the pre-rename `"sglake"`. By the time you
    /// read this the value has already been normalized to `"aglake"`, so the
    /// runtime behaves identically; it is kept only so `validate()` can tell
    /// the operator to update the file. See [`ConfigIssue::LegacyBackendName`].
    #[serde(skip)]
    pub legacy_backend_name: Option<String>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            duckdb: DuckDbConfig::default(),
            clickhouse: ClickHouseConfig::default(),
            aglake: AglakeConfig::default(),
            sink: StorageSinkConfig::default(),
            retention: RetentionConfig::default(),
            legacy_backend_name: None,
        }
    }
}

/// Default per-granularity retention for `llm_metrics`, in days.
///
/// The single source of truth for **known granularity labels** — the keys must
/// match the labels produced by `h-metrics::aggregator::GRANULARITIES`. Used
/// at config-load time to populate any granularity the user did not override
/// and to drop typos like `"10sec"` with a warning. Adding a new granularity
/// requires updating h-metrics + this single constant.
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
    /// Max age in days for `spans` (formerly `calls`). `0` = never expire.
    /// The legacy `calls` key is still accepted via serde alias.
    #[serde(default = "default_spans_retention_days", alias = "calls")]
    pub spans: u32,
    /// Max age in days for `traces` (formerly `turns`). `0` = never expire.
    /// The legacy `turns` key is still accepted via serde alias.
    #[serde(default = "default_traces_retention_days", alias = "turns")]
    pub traces: u32,
    /// Max age in days for `http_exchanges`. `0` = never expire. Raw headers +
    /// bodies make this the bulkiest table, so a short forensics window keeps
    /// storage bounded.
    #[serde(default = "default_http_exchanges_retention_days")]
    pub http_exchanges: u32,
    /// Per-granularity retention overrides for `llm_metrics`, in days. Key =
    /// granularity label (`"10s"`, `"1m"`, `"5m"`, `"1h"`). Missing keys fall
    /// back to defaults defined in `h-storage::retention`; set a key to `0`
    /// to disable retention for that granularity.
    #[serde(default)]
    pub metrics: HashMap<String, u32>,
    /// Granularity labels in `metrics` that didn't match any known label
    /// (typo guard). Populated by [`resolve_metrics_retention`] at load time
    /// from the user's raw input — by the time you read this, the unknowns
    /// have already been dropped from `metrics`. Surfaced by
    /// `AppConfig::validate()` so `heron config validate` can fail
    /// loudly on typos that the load-time warn easily missed.
    #[serde(skip)]
    pub unknown_granularities: Vec<String>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: default_retention_enabled(),
            check_interval_secs: default_retention_check_interval_secs(),
            spans: default_spans_retention_days(),
            traces: default_traces_retention_days(),
            http_exchanges: default_http_exchanges_retention_days(),
            metrics: HashMap::new(),
            unknown_granularities: Vec::new(),
        }
    }
}

fn default_retention_enabled() -> bool {
    true
}

fn default_spans_retention_days() -> u32 {
    30
}

fn default_traces_retention_days() -> u32 {
    // Must satisfy traces <= spans (see ConfigIssue::TracesRetentionExceedsSpans).
    // Kept equal to spans so the default deploy is consistent without forcing
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

/// `storage.backend` value selecting the Aglake backend.
pub const AGLAKE_BACKEND: &str = "aglake";

/// What that same backend was called before the upstream project renamed
/// itself to Aglake in its 0.3 release. Accepted on input and normalized to
/// [`AGLAKE_BACKEND`]; see [`StorageConfig::legacy_backend_name`].
pub const LEGACY_AGLAKE_BACKEND: &str = "sglake";

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
    "data/heron.duckdb".to_string()
}

/// Connection + behaviour settings for the ClickHouse storage backend. Only
/// read when `storage.backend == "clickhouse"`. The `clickhouse` crate talks
/// to the server over its HTTP interface (default port 8123).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClickHouseConfig {
    /// HTTP endpoint, e.g. `http://clickhouse:8123`. The `clickhouse` crate
    /// also accepts an `https://` URL.
    #[serde(default = "default_clickhouse_url")]
    pub url: String,
    /// Target database; created on `init()` if absent.
    #[serde(default = "default_clickhouse_database")]
    pub database: String,
    #[serde(default = "default_clickhouse_user")]
    pub user: String,
    #[serde(default)]
    pub password: String,
    /// Run `OPTIMIZE TABLE ... FINAL` after each retention sweep to reclaim
    /// space eagerly. Off by default — TTL-driven background merges reclaim
    /// space lazily and `OPTIMIZE FINAL` is expensive on large tables.
    #[serde(default)]
    pub optimize_on_sweep: bool,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: default_clickhouse_url(),
            database: default_clickhouse_database(),
            user: default_clickhouse_user(),
            password: String::new(),
            optimize_on_sweep: false,
        }
    }
}

fn default_clickhouse_url() -> String {
    "http://localhost:8123".to_string()
}

fn default_clickhouse_database() -> String {
    "heron".to_string()
}

fn default_clickhouse_user() -> String {
    "default".to_string()
}

/// Connection + behaviour settings for the Aglake storage backend.
/// Only read when `storage.backend == "aglake"`.
///
/// Writes go through the Splunk-compatible HEC (`/services/collector/event`);
/// reads are SPL over `/api/v1/search`. Heron's five tables map onto a set of
/// aglake indexes under `index_prefix`: `_spans` / `_bodies` / `_traces` /
/// `_metrics_<granularity>` / `_finish_<granularity>` / `_http` /
/// `_http_bodies`. Bodies live in their own indexes so list and aggregate
/// queries never touch body bytes, and so bodies can expire earlier than the
/// metadata that references them.
///
/// ⚠️ Security: HEC and `/api/v1/*` authenticate **separately**. `hec_token`
/// covers ingest only; the search and admin faces are open until aglaked has a
/// user catalog, and then need a session (`username`/`password`, or
/// `session_token`). aglaked serves no HTTPS of its own either way, so a
/// non-loopback link belongs on a trusted network or behind a reverse proxy.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AglakeConfig {
    /// aglaked base URL, e.g. `http://127.0.0.1:5959`.
    #[serde(default = "default_aglake_url")]
    pub url: String,
    /// HEC token. Only needed when aglaked runs with `--hec-token`; the header
    /// is matched exactly as `Authorization: Splunk <token>`.
    ///
    /// This authenticates ingest only. It does **not** open `/api/v1/*` —
    /// those need a session, from `username`/`password` or `session_token`.
    #[serde(default)]
    pub hec_token: String,
    /// Username in aglake's local user catalog, exchanged for a session at
    /// first use. Leave empty when aglaked runs with no users (its default),
    /// where `/api/v1/*` is open and no credentials are sent.
    ///
    /// The account needs the `admin` role only if `manage_retention` is on —
    /// pushing per-index TTLs goes through `/api/v1/admin/*`. A search-only
    /// deployment can use an unprivileged account.
    #[serde(default)]
    pub username: String,
    /// Password for [`AglakeConfig::username`].
    #[serde(default)]
    pub password: String,
    /// An existing session token, presented as `Authorization: Bearer`,
    /// instead of logging in. Takes precedence over `username`/`password`.
    ///
    /// Sessions live in the daemon's memory with a 12-hour TTL and are lost
    /// when it restarts, and Heron cannot mint a new one from a token alone —
    /// so this is for short-lived or externally-refreshed setups. Prefer
    /// `username`/`password` for anything long-running.
    #[serde(default)]
    pub session_token: String,
    /// Prefix for every index this backend owns. Must avoid aglake's built-in
    /// names (`main` / `traces` / `metrics` / `summary` / `_internal` / `_audit`)
    /// — note `traces` in particular is already taken by OTLP spans.
    #[serde(default = "default_aglake_index_prefix")]
    pub index_prefix: String,
    /// Persist request/response bodies and headers. `false` keeps only
    /// metadata, which is the cheapest possible footprint.
    #[serde(default = "default_true")]
    pub store_bodies: bool,
    /// Retention for the body indexes, in days. `0` inherits
    /// `storage.retention.spans`.
    #[serde(default)]
    pub body_retention_days: u32,
    /// Push per-index retention to aglake's management API on
    /// `apply_retention`. When false the call is a no-op and retention is left
    /// to whoever operates aglaked.
    #[serde(default = "default_true")]
    pub manage_retention: bool,
    /// Max bytes per HEC request, pre-compression. Must stay below aglaked's
    /// `--max-body-mib` (default 100 MiB) — the limit is enforced on the
    /// *decompressed* size, so gzip does not buy headroom.
    #[serde(default = "default_aglake_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Hard ceiling for a single event, pre-compression. Must stay below
    /// aglake's 16 MiB WAL frame limit: an oversized event is discarded as
    /// corruption during crash replay, which is the worst failure mode there
    /// is. `[body_cap]` normally keeps bodies far below this; this is the only
    /// guard when `body_cap.enabled = false`.
    #[serde(default = "default_aglake_max_event_bytes")]
    pub max_event_bytes: usize,
    /// gzip HEC request bodies.
    #[serde(default = "default_true")]
    pub gzip: bool,
    /// Use HEC indexer acknowledgement to avoid duplicate writes when a
    /// request fails after the server already committed it.
    #[serde(default = "default_true")]
    pub use_ack: bool,
    #[serde(default = "default_aglake_write_retries")]
    pub write_retries: u32,
    #[serde(default = "default_aglake_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
    #[serde(default = "default_aglake_request_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_aglake_search_timeout_secs")]
    pub search_timeout_secs: u64,
    /// Deep-pagination ceiling. SPL has no offset/cursor, so a page at offset
    /// N costs `sort N`; past this the backend errors out rather than silently
    /// truncating.
    #[serde(default = "default_aglake_max_page_offset")]
    pub max_page_offset: u64,
    /// Guard for the session-list scan, which must materialize one row per
    /// session in the window before it can page.
    #[serde(default = "default_aglake_max_sessions_scan")]
    pub max_sessions_scan: u64,
    /// Concurrency limit for the multi-request read paths (id-chunked point
    /// lookups, the three-step session list).
    #[serde(default = "default_aglake_max_concurrent_searches")]
    pub max_concurrent_searches: usize,
    /// A trace's `_time` is its start; queries that filter on end time widen
    /// the search window by this much so bucket pruning stays correct.
    #[serde(default = "default_aglake_trace_time_skew_hours")]
    pub trace_time_skew_hours: u32,
    /// Deduplicate metric rows on read by `row_id`. Off by default: writes are
    /// at-least-once but duplicates are rare, and `dedup` costs a full sort.
    /// Turn it on if duplicate metric rows are ever observed.
    #[serde(default)]
    pub metrics_dedup: bool,
    /// Emulate updates to `traces` by appending a new revision and
    /// deduplicating on read. Off by default because it forces every traces
    /// read onto a full-window sort, which defeats the pagination design; see
    /// the crate docs for what stays broken while it is off (proxy pairing).
    #[serde(default)]
    pub enable_trace_patching: bool,
}

impl Default for AglakeConfig {
    fn default() -> Self {
        Self {
            url: default_aglake_url(),
            hec_token: String::new(),
            username: String::new(),
            password: String::new(),
            session_token: String::new(),
            index_prefix: default_aglake_index_prefix(),
            store_bodies: true,
            body_retention_days: 0,
            manage_retention: true,
            max_body_bytes: default_aglake_max_body_bytes(),
            max_event_bytes: default_aglake_max_event_bytes(),
            gzip: true,
            use_ack: true,
            write_retries: default_aglake_write_retries(),
            retry_backoff_ms: default_aglake_retry_backoff_ms(),
            request_timeout_secs: default_aglake_request_timeout_secs(),
            search_timeout_secs: default_aglake_search_timeout_secs(),
            max_page_offset: default_aglake_max_page_offset(),
            max_sessions_scan: default_aglake_max_sessions_scan(),
            max_concurrent_searches: default_aglake_max_concurrent_searches(),
            trace_time_skew_hours: default_aglake_trace_time_skew_hours(),
            metrics_dedup: false,
            enable_trace_patching: false,
        }
    }
}

fn default_aglake_url() -> String {
    "http://127.0.0.1:5959".to_string()
}

fn default_aglake_index_prefix() -> String {
    "heron".to_string()
}

fn default_aglake_max_body_bytes() -> usize {
    32 * 1024 * 1024
}

fn default_aglake_max_event_bytes() -> usize {
    8 * 1024 * 1024
}

fn default_aglake_write_retries() -> u32 {
    3
}

fn default_aglake_retry_backoff_ms() -> u64 {
    200
}

fn default_aglake_request_timeout_secs() -> u64 {
    120
}

fn default_aglake_search_timeout_secs() -> u64 {
    120
}

fn default_aglake_max_page_offset() -> u64 {
    100_000
}

fn default_aglake_max_sessions_scan() -> u64 {
    200_000
}

fn default_aglake_max_concurrent_searches() -> usize {
    8
}

fn default_aglake_trace_time_skew_hours() -> u32 {
    24
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
    // typical mid-size deployment workload). At 200 ms the writer fires ~5×
    // more often than at 1000 ms; each flush is ~5 ms (DuckDB appender),
    // so the extra wall-clock cost stays under 3 %.
    200
}

/// Severity of a [`ConfigIssue`]. `Error` blocks `heron config validate`
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
/// `code`) so `heron config validate` and `heron doctor` produce
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
    /// no-JOIN turn-detail read (`agent_turns.span_ids` → `llm_calls`
    /// IN-lookup) returns empty/partial calls for surviving turns once the
    /// calls sweep crosses their `request_time`. `traces_days = 0` is the
    /// sentinel for "never expire" (which always violates a finite
    /// `spans_days`); finite-vs-finite triggers when `traces_days > spans_days`.
    /// Only emitted when `spans_days > 0` — infinite calls retention can
    /// satisfy any turns retention.
    TracesRetentionExceedsSpans { traces_days: u32, spans_days: u32 },
    /// `storage.aglake.index_prefix` is not a usable index-name token, so the
    /// backend would write under names nobody expects — an empty prefix in
    /// particular lands in aglake's own leading-underscore namespace. Only
    /// emitted when `storage.backend == "aglake"`.
    AglakeReservedIndexPrefix { prefix: String, index: String },
    /// `storage.aglake.max_event_bytes` is smaller than a single capped body
    /// can be, so full-size events are dropped before they are ever sent.
    /// That is silent data loss at steady state, not an edge case. Only
    /// emitted when `storage.backend == "aglake"`.
    AglakeEventCapBelowBodyCap {
        max_event_bytes: usize,
        body_cap_bytes: usize,
    },
    /// `storage.aglake.url` names a host other than loopback.
    AglakeUrlNotLoopback { url: String, host: String },
    /// Body indexes are set to outlive the span metadata that points at them,
    /// leaving bodies nothing can reach. `0` means "inherit", which is always
    /// consistent. Only emitted when `storage.backend == "aglake"`.
    AglakeBodyRetentionExceedsParent {
        body_days: u32,
        /// Which entity's retention the bodies would outlive — `spans` for the
        /// LLM-call bodies index, `http_exchanges` for the HTTP one.
        parent: String,
        parent_days: u32,
    },
    /// `storage.backend` (or `[storage.sglake]`) still spells the backend with
    /// its pre-0.3 name. Already normalized at load time, so the runtime is
    /// unaffected — reported so the file gets updated before the alias is
    /// eventually dropped.
    LegacyBackendName { found: String, use_instead: String },
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
            | Self::PcapDumpRetentionNoRules { .. }
            // Orphaned bodies waste space but break nothing: the metadata
            // rows that would reference them are already gone.
            | Self::AglakeUrlNotLoopback { .. }
            // The alias still resolves, so this deployment runs correctly; it
            // just names something that no longer exists upstream.
            | Self::LegacyBackendName { .. }
            | Self::AglakeBodyRetentionExceedsParent { .. } => IssueSeverity::Warn,
            Self::DuplicatePipelineName(_)
            | Self::DuplicateSourceId { .. }
            | Self::StoragePathParentUnwritable { .. }
            | Self::UnknownRetentionGranularity(_)
            | Self::UnsafePcapDumpPipelineName { .. }
            | Self::TracesRetentionExceedsSpans { .. }
            | Self::AglakeReservedIndexPrefix { .. }
            | Self::AglakeEventCapBelowBodyCap { .. } => IssueSeverity::Error,
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
            Self::TracesRetentionExceedsSpans {
                traces_days,
                spans_days,
            } => {
                let turns_str = if *traces_days == 0 {
                    "never expire".to_string()
                } else {
                    format!("{traces_days}d")
                };
                write!(
                    f,
                    "storage.retention.traces ({turns_str}) outlives \
                     storage.retention.spans ({spans_days}d): traces whose \
                     spans have been pruned will show empty/partial span \
                     lists. Set traces <= spans (or set spans = 0 for infinite)."
                )
            }
            Self::AglakeReservedIndexPrefix { prefix, index } => write!(
                f,
                "storage.aglake.index_prefix '{prefix}' is not a usable index \
                 name token (it would produce '{index}'). Use lowercase \
                 letters, digits and underscores, and do not start with '_' \
                 — that is aglake's own namespace."
            ),
            Self::AglakeEventCapBelowBodyCap {
                max_event_bytes,
                body_cap_bytes,
            } => write!(
                f,
                "storage.aglake.max_event_bytes ({max_event_bytes}) is below \
                 the {body_cap_bytes} bytes a capped body can reach, so \
                 full-size events would be dropped before being sent. Raise \
                 max_event_bytes or lower [body_cap]."
            ),
            Self::AglakeUrlNotLoopback { url, host } => write!(
                f,
                "storage.aglake.url points at '{host}' ({url}) with no \
                 credentials configured, so nothing but the network stands in \
                 front of the /api/v1/* search endpoints — every stored \
                 request and response body is readable by anyone who can reach \
                 that port, and Heron cannot restrict it. Give aglaked a user \
                 catalog and set storage.aglake.username / password, bind it \
                 to 127.0.0.1, or put the link on a network only Heron can use."
            ),
            Self::AglakeBodyRetentionExceedsParent {
                body_days,
                parent,
                parent_days,
            } => write!(
                f,
                "storage.aglake.body_retention_days ({body_days}d) outlives \
                 storage.retention.{parent} ({parent_days}d): bodies will \
                 survive the metadata that points at them and become \
                 unreachable, since every read finds a body through its \
                 parent's id. Set body_retention_days <= {parent} (or 0 to \
                 inherit each body index's own parent)."
            ),
            Self::LegacyBackendName { found, use_instead } => write!(
                f,
                "'{found}' is what the storage backend was called before the \
                 upstream project renamed itself to Aglake in 0.3. It is still \
                 accepted and this deployment runs unaffected, but the name is \
                 gone upstream: set storage.backend = \"{use_instead}\" and \
                 rename the [storage.{found}] table to [storage.{use_instead}]."
            ),
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
/// Host portion of a `scheme://host[:port][/path]` URL, lowercased.
///
/// Deliberately not a URL parser: the only question is which host the operator
/// named, and pulling in a dependency to answer it — or failing closed on a
/// shape a real parser would reject — would both be worse than saying nothing.
/// An unparseable URL returns `None` and is left to fail at connect time with
/// a message about the actual problem.
fn aglake_url_host(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip userinfo, then the port — but not the colons inside a bracketed
    // IPv6 literal.
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(end) = authority.find(']') {
        &authority[..=end]
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether a host names this machine only.
fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare == "localhost" {
        return true;
    }
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A name that is not an address could resolve anywhere; only the
        // conventional one is treated as local.
        Err(_) => false,
    }
}

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
    let probe = probe_root.join(format!(".heron_validate_probe.{}", std::process::id()));
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
    /// `heron config validate` and `heron doctor`.
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
            if def.pcap_dump.enabled && !crate::path::is_safe_path_component(&def.name) {
                issues.push(ConfigIssue::UnsafePcapDumpPipelineName {
                    pipeline: def.name.clone(),
                });
            }
        }

        for unknown in &self.storage.retention.unknown_granularities {
            issues.push(ConfigIssue::UnknownRetentionGranularity(unknown.clone()));
        }

        // agent_turns references llm_calls via JSON span_ids; the no-JOIN
        // turn-detail read trusts that referenced calls still exist. If turns
        // outlive calls, surviving turns end up pointing at deleted call ids
        // and the detail view shows empty/partial results. 0 = never expire,
        // so finite spans_days with any larger (or 0) traces_days is broken.
        let spans_days = self.storage.retention.spans;
        let traces_days = self.storage.retention.traces;
        if spans_days > 0 && (traces_days == 0 || traces_days > spans_days) {
            issues.push(ConfigIssue::TracesRetentionExceedsSpans {
                traces_days,
                spans_days,
            });
        }

        if let Some(found) = &self.storage.legacy_backend_name {
            issues.push(ConfigIssue::LegacyBackendName {
                found: found.clone(),
                use_instead: AGLAKE_BACKEND.to_string(),
            });
        }

        if self.storage.backend == AGLAKE_BACKEND {
            let sg = &self.storage.aglake;
            // aglake has no DDL — an index exists because something wrote to
            // it — so a malformed prefix is never rejected by the store. It
            // just starts writing under a name nobody expects. The exact
            // reserved-name collision check lives in the backend, which owns
            // index naming; what this layer can check is that the prefix is a
            // usable token at all. An empty prefix in particular yields
            // `_spans`, and leading-underscore indexes are aglake's own
            // namespace.
            let prefix = sg.index_prefix.as_str();
            if prefix.is_empty()
                || !prefix
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                || prefix.starts_with('_')
            {
                issues.push(ConfigIssue::AglakeReservedIndexPrefix {
                    prefix: sg.index_prefix.clone(),
                    index: format!("{prefix}_spans"),
                });
            }
            // Until aglake 0.3 the search API had no authentication of its
            // own, which made where it listens the entire access-control story
            // for the bodies Heron stores there. Credentials change that: with
            // a session configured, the daemon has a user catalog and the port
            // is no longer the only thing standing in front of the data, so
            // the warning would be noise. Without them, the reachable-port
            // risk is exactly what it always was — Heron does not start
            // aglaked and cannot bind it, so this stays the one place it can
            // be named.
            let has_session = !sg.username.is_empty() || !sg.session_token.is_empty();
            if !has_session {
                if let Some(host) = aglake_url_host(&sg.url) {
                    if !is_loopback_host(&host) {
                        issues.push(ConfigIssue::AglakeUrlNotLoopback {
                            url: sg.url.clone(),
                            host,
                        });
                    }
                }
            }
            // A body at the cap plus its headers and JSON escaping has to fit
            // in one event, or the write path drops it every time.
            if self.body_cap.enabled {
                let body_cap_bytes = self.body_cap.head_bytes + self.body_cap.tail_bytes;
                if sg.store_bodies && sg.max_event_bytes < body_cap_bytes {
                    issues.push(ConfigIssue::AglakeEventCapBelowBodyCap {
                        max_event_bytes: sg.max_event_bytes,
                        body_cap_bytes,
                    });
                }
            }
            // Bodies live in their own indexes and, when given an explicit
            // retention, use it for both of them. Outliving *either* parent
            // strands bodies that nothing can reach — a body is only ever
            // found through its parent's id.
            for (parent, days) in [
                ("spans", self.storage.retention.spans),
                ("http_exchanges", self.storage.retention.http_exchanges),
            ] {
                if days > 0 && sg.body_retention_days > days {
                    issues.push(ConfigIssue::AglakeBodyRetentionExceedsParent {
                        body_days: sg.body_retention_days,
                        parent: parent.to_string(),
                        parent_days: days,
                    });
                }
            }
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
        assert_eq!(cfg.spans, 30);
        // turns must not exceed calls — see ConfigIssue::TracesRetentionExceedsSpans.
        assert_eq!(cfg.traces, 30);
        assert_eq!(cfg.http_exchanges, 7);
        // metrics map stays empty — per-granularity defaults are merged at
        // policy-build time in h-storage so users can override one label
        // without dropping the rest.
        assert!(cfg.metrics.is_empty());
    }

    #[test]
    fn storage_config_embeds_retention_defaults() {
        let cfg = StorageConfig::default();
        assert!(cfg.retention.enabled);
        assert_eq!(cfg.retention.spans, 30);
        // turns must not exceed calls — see ConfigIssue::TracesRetentionExceedsSpans.
        assert_eq!(cfg.retention.traces, 30);
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
        assert_eq!(cfg.spans, 7);
        assert_eq!(cfg.traces, 30);
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
            loop_count: 1,
            loop_secs: 0,
            rate_pps: 0,
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
        assert_eq!(cfg.raw_pkts, 4096);
        assert_eq!(cfg.parsed_pkts, 4096);
        assert_eq!(cfg.http_parse_events, 4096);
        assert_eq!(cfg.http_joiner_events, 4096);
        assert_eq!(cfg.agent_calls, 4096);
        assert_eq!(cfg.llm_events, 4096);
        assert_eq!(cfg.storage_calls, 4096);
        assert_eq!(cfg.storage_turns, 4096);
        assert_eq!(cfg.storage_metrics, 4096);
        assert_eq!(cfg.storage_exchanges, 4096);
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
        // `/proc/heron-validate-test` exists on Linux but is not writable;
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
                ConfigIssue::TracesRetentionExceedsSpans {
                    traces_days: 30,
                    spans_days: 7
                }
            )),
            "expected TracesRetentionExceedsSpans(30, 7), got {issues:?}"
        );
    }

    #[test]
    fn validate_turns_infinite_with_calls_finite_is_error() {
        // turns = 0 (never expire) and calls > 0 → turns outlive every call.
        // Detected as the same issue (sentinel traces_days = 0).
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
                ConfigIssue::TracesRetentionExceedsSpans {
                    traces_days: 0,
                    spans_days: 7
                }
            )),
            "expected TracesRetentionExceedsSpans(0, 7), got {issues:?}"
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
                .any(|i| matches!(i, ConfigIssue::TracesRetentionExceedsSpans { .. })),
            "expected no TracesRetentionExceedsSpans, got {issues:?}"
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
                .any(|i| matches!(i, ConfigIssue::TracesRetentionExceedsSpans { .. })),
            "expected no TracesRetentionExceedsSpans, got {issues:?}"
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
                .any(|i| matches!(i, ConfigIssue::TracesRetentionExceedsSpans { .. })),
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
            path = "{}/heron-validate-clean.duckdb"
            "#,
            tmp.display()
        );
        let cfg = AppConfig::from_toml(&toml);
        let issues = cfg.validate();
        assert!(issues.is_empty(), "expected no issues, got {issues:?}");
    }

    /// The aglake checks must stay dormant for every other backend —
    /// `[storage.aglake]` carries defaults whether or not it is in use.
    #[test]
    fn validate_skips_aglake_checks_on_other_backends() {
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "clickhouse"

            [storage.aglake]
            index_prefix = ""
            max_event_bytes = 1
            "#,
        );
        assert!(
            !cfg.validate().iter().any(|i| matches!(
                i,
                ConfigIssue::AglakeReservedIndexPrefix { .. }
                    | ConfigIssue::AglakeEventCapBelowBodyCap { .. }
            )),
            "aglake issues leaked into a clickhouse config"
        );
    }

    /// An empty prefix yields `_spans`, which sits in aglake's own
    /// leading-underscore namespace.
    #[test]
    fn validate_rejects_unusable_aglake_index_prefix() {
        for prefix in ["", "_hidden", "Heron", "he ron", "heron-1"] {
            let cfg = AppConfig::from_toml(&format!(
                r#"
                [[pipeline]]
                name = "p"
                [[pipeline.sources]]
                type = "pcap"
                interface = "eth0"

                [storage]
                backend = "aglake"

                [storage.aglake]
                index_prefix = "{prefix}"
                "#
            ));
            assert!(
                cfg.validate()
                    .iter()
                    .any(|i| matches!(i, ConfigIssue::AglakeReservedIndexPrefix { .. })),
                "accepted unusable prefix {prefix:?}"
            );
        }

        // The default prefix, and other plain tokens, must pass.
        for prefix in ["heron", "heron_prod", "h2"] {
            let cfg = AppConfig::from_toml(&format!(
                r#"
                [[pipeline]]
                name = "p"
                [[pipeline.sources]]
                type = "pcap"
                interface = "eth0"

                [storage]
                backend = "aglake"

                [storage.aglake]
                index_prefix = "{prefix}"
                "#
            ));
            assert!(
                !cfg.validate()
                    .iter()
                    .any(|i| matches!(i, ConfigIssue::AglakeReservedIndexPrefix { .. })),
                "rejected valid prefix {prefix:?}"
            );
        }
    }

    /// A config file written before the upstream rename keeps working: the
    /// backend value is normalized and the old `[storage.sglake]` table lands
    /// on the same struct, so nothing downstream sees two spellings. The only
    /// visible difference is one warning telling the operator to update it.
    #[test]
    fn legacy_sglake_names_load_and_normalize() {
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "sglake"

            [storage.sglake]
            index_prefix = "legacy"
            max_page_offset = 4242
            "#,
        );

        assert_eq!(cfg.storage.backend, AGLAKE_BACKEND);
        // The aliased table populated the real field, not a default.
        assert_eq!(cfg.storage.aglake.index_prefix, "legacy");
        assert_eq!(cfg.storage.aglake.max_page_offset, 4242);

        let issues = cfg.validate();
        let legacy: Vec<_> = issues
            .iter()
            .filter(|i| matches!(i, ConfigIssue::LegacyBackendName { .. }))
            .collect();
        assert_eq!(legacy.len(), 1, "expected exactly one deprecation notice");
        // Warn, not Error: the deployment runs correctly on the alias.
        assert_eq!(legacy[0].severity(), IssueSeverity::Warn);
    }

    /// The current spelling must not trip the deprecation notice.
    #[test]
    fn current_aglake_name_is_not_flagged_as_legacy() {
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "aglake"

            [storage.aglake]
            index_prefix = "heron"
            "#,
        );

        assert_eq!(cfg.storage.backend, AGLAKE_BACKEND);
        assert!(cfg.storage.legacy_backend_name.is_none());
        assert!(!cfg
            .validate()
            .iter()
            .any(|i| matches!(i, ConfigIssue::LegacyBackendName { .. })));
    }

    /// An event ceiling below the body cap drops every full-size body, every
    /// time — silent steady-state data loss, so it has to be an error.
    #[test]
    fn validate_flags_event_cap_below_body_cap() {
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "aglake"

            [storage.aglake]
            max_event_bytes = 1024

            [body_cap]
            enabled = true
            head_bytes = 262144
            tail_bytes = 65536
            "#,
        );
        let issue = cfg
            .validate()
            .into_iter()
            .find(|i| matches!(i, ConfigIssue::AglakeEventCapBelowBodyCap { .. }))
            .expect("expected the event-cap issue");
        assert_eq!(issue.severity(), IssueSeverity::Error);
        assert!(issue.to_string().contains("max_event_bytes"));

        // Storing no bodies removes the constraint entirely.
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "aglake"

            [storage.aglake]
            max_event_bytes = 1024
            store_bodies = false

            [body_cap]
            enabled = true
            head_bytes = 262144
            tail_bytes = 65536
            "#,
        );
        assert!(!cfg
            .validate()
            .iter()
            .any(|i| matches!(i, ConfigIssue::AglakeEventCapBelowBodyCap { .. })));
    }

    /// Bodies outliving their span metadata is wasteful but not broken, so it
    /// warns rather than failing validation.
    #[test]
    fn validate_warns_on_orphaned_body_retention() {
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "aglake"

            [storage.aglake]
            body_retention_days = 90

            [storage.retention]
            spans = 7
            traces = 7
            http_exchanges = 3
            "#,
        );
        let issues: Vec<_> = cfg
            .validate()
            .into_iter()
            .filter(|i| matches!(i, ConfigIssue::AglakeBodyRetentionExceedsParent { .. }))
            .collect();
        // Bodies sit in two indexes with two different parents; outliving
        // either one strands bodies, so both have to be reported.
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(issues.iter().all(|i| i.severity() == IssueSeverity::Warn));
        let named: Vec<&str> = issues
            .iter()
            .map(|i| match i {
                ConfigIssue::AglakeBodyRetentionExceedsParent { parent, .. } => parent.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(named, vec!["spans", "http_exchanges"]);

        // 0 means "inherit", which can never conflict.
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "aglake"

            [storage.aglake]
            body_retention_days = 0

            [storage.retention]
            spans = 7
            traces = 7
            "#,
        );
        assert!(!cfg
            .validate()
            .iter()
            .any(|i| matches!(i, ConfigIssue::AglakeBodyRetentionExceedsParent { .. })));
    }

    #[test]
    fn loopback_urls_are_accepted_and_anything_else_is_flagged() {
        for url in [
            "http://127.0.0.1:5959",
            "http://localhost:5959",
            "https://LOCALHOST/",
            "http://[::1]:5959",
            "http://127.5.6.7:5959",
            "http://user:pw@127.0.0.1:5959/x",
        ] {
            let host = aglake_url_host(url).unwrap_or_else(|| panic!("no host in {url}"));
            assert!(
                is_loopback_host(&host),
                "{url} -> {host} should be loopback"
            );
        }
        for url in [
            "http://10.0.0.5:5959",
            "http://aglake.internal:5959",
            "http://[2001:db8::1]:5959",
            "http://0.0.0.0:5959",
        ] {
            let host = aglake_url_host(url).unwrap_or_else(|| panic!("no host in {url}"));
            assert!(
                !is_loopback_host(&host),
                "{url} -> {host} should not be loopback"
            );
        }
        // Unparseable input says nothing rather than guessing.
        assert_eq!(aglake_url_host("not a url"), None);
    }

    /// With no credentials configured, where aglaked listens is the only thing
    /// standing between stored request bodies and the network.
    #[test]
    fn a_non_loopback_aglake_url_is_warned_about() {
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "aglake"

            [storage.aglake]
            url = "http://10.0.0.5:5959"
            "#,
        );
        let issue = cfg
            .validate()
            .into_iter()
            .find(|i| matches!(i, ConfigIssue::AglakeUrlNotLoopback { .. }))
            .expect("expected the loopback warning");
        assert_eq!(issue.severity(), IssueSeverity::Warn);
        assert!(
            issue.to_string().contains("no credentials configured"),
            "the message must say why: {issue}"
        );

        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "aglake"

            [storage.aglake]
            url = "http://127.0.0.1:5959"
            "#,
        );
        assert!(!cfg
            .validate()
            .iter()
            .any(|i| matches!(i, ConfigIssue::AglakeUrlNotLoopback { .. })));
    }

    /// Credentials answer what the loopback warning is about: with a session
    /// configured, the port is no longer the only thing in front of the data,
    /// so a remote aglake is a legitimate deployment rather than a finding.
    #[test]
    fn configured_credentials_lift_the_loopback_warning() {
        let remote = |credentials: &str| {
            AppConfig::from_toml(&format!(
                r#"
                [[pipeline]]
                name = "p"
                [[pipeline.sources]]
                type = "pcap"
                interface = "eth0"

                [storage]
                backend = "aglake"

                [storage.aglake]
                url = "http://10.0.0.5:5959"
                {credentials}
                "#
            ))
        };

        for credentials in [
            r#"username = "heron""#,
            r#"session_token = "deadbeef""#,
            // A password without a username is not a credential — nothing to
            // log in as — so it must not silence the warning.
            "",
        ] {
            let warned = remote(credentials)
                .validate()
                .iter()
                .any(|i| matches!(i, ConfigIssue::AglakeUrlNotLoopback { .. }));
            assert_eq!(
                warned,
                credentials.is_empty(),
                "unexpected warning state for {credentials:?}"
            );
        }
    }

    /// Only when aglake is the active backend — an unused `[storage.aglake]`
    /// block is not a finding.
    #[test]
    fn the_loopback_warning_is_scoped_to_the_active_backend() {
        let cfg = AppConfig::from_toml(
            r#"
            [[pipeline]]
            name = "p"
            [[pipeline.sources]]
            type = "pcap"
            interface = "eth0"

            [storage]
            backend = "duckdb"

            [storage.aglake]
            url = "http://10.0.0.5:5959"
            "#,
        );
        assert!(!cfg
            .validate()
            .iter()
            .any(|i| matches!(i, ConfigIssue::AglakeUrlNotLoopback { .. })));
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
        // hard error so `heron config validate` catches it before
        // deploy. We test '..' specifically; other unsafe shapes share
        // the same code path (covered by h-common::path tests).
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
