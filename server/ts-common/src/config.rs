use std::collections::HashMap;
use std::path::Path;

use config::Config;
use serde::Deserialize;

use crate::error::AppError;

/// Top-level application configuration.
///
/// Not directly deserializable — use [`AppConfig::load`] or [`AppConfig::from_toml`]
/// which go through [`RawAppConfig`] two-phase parsing.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub pipelines: Vec<PipelineDef>,
    pub storage: StorageConfig,
    pub internal_metrics: InternalMetricsConfig,
    pub api: ApiConfig,
}

/// A single pipeline definition bundling sources and pipeline parameters.
#[derive(Debug, Clone, Deserialize)]
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
        AppConfig {
            pipelines,
            storage: self.storage,
            internal_metrics: self.internal_metrics,
            api: self.api,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaptureConfig {
    #[serde(default)]
    pub sources: Vec<CaptureSourceConfig>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
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
        stream_id: Option<String>,
    },
    PcapFile {
        path: String,
        #[serde(default)]
        realtime: bool,
        #[serde(default)]
        stream_id: Option<String>,
    },
    CloudProbe {
        #[serde(default = "default_cloud_probe_endpoint")]
        endpoint: String,
        #[serde(default = "default_cloud_probe_hwm")]
        recv_hwm: i32,
    },
}

impl CaptureSourceConfig {
    /// Resolve the stream_id for this source. Returns `Some` for static sources
    /// (pcap, pcap-file) with a default derived from interface/filename.
    /// Returns `None` for cloud-probe (stream_id comes from batch UUID at runtime).
    pub fn resolved_stream_id(&self) -> Option<String> {
        match self {
            Self::Pcap {
                stream_id,
                interface,
                ..
            } => Some(stream_id.clone().unwrap_or_else(|| interface.clone())),
            Self::PcapFile {
                stream_id, path, ..
            } => {
                let base = std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path)
                    .to_string();
                Some(stream_id.clone().unwrap_or(base))
            }
            Self::CloudProbe { .. } => None,
        }
    }
}

fn default_interface() -> String {
    "eth0".to_string()
}

fn default_snaplen() -> u32 {
    65535
}

fn default_cloud_probe_endpoint() -> String {
    "tcp://0.0.0.0:5555".to_string()
}

fn default_cloud_probe_hwm() -> i32 {
    1000
}

#[derive(Debug, Clone, Deserialize)]
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
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            dispatcher_count: default_dispatcher_count(),
            flow_shard_count: default_flow_shard_count(),
            queues: QueueConfig::default(),
            turn: TurnConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }
}

fn default_flow_shard_count() -> usize {
    4
}

/// Capacities of every bounded `mpsc` channel sitting between pipeline stages.
/// All default to 4096 — override individually under `[pipeline.queues]`.
#[derive(Debug, Clone, Deserialize)]
pub struct QueueConfig {
    /// capture → flow dispatcher
    #[serde(default = "default_queue_capacity")]
    pub raw: usize,
    /// flow dispatcher → each protocol parser shard (ParsedPacket)
    #[serde(default = "default_queue_capacity")]
    pub parsed_packet: usize,
    /// protocol parser → llm stage (ProtocolEvent, per shard)
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
    /// turn stage → storage sink (LlmTurn records)
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

#[derive(Debug, Clone, Deserialize)]
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

/// Data retention policy for stored telemetry. Disabled by default; enable and
/// set per-table TTLs to have old rows periodically deleted.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_retention_check_interval_secs")]
    pub check_interval_secs: u64,
    /// Max age in days for `llm_calls`. `0` (or absent) = never expire.
    #[serde(default)]
    pub calls: u32,
    /// Max age in days for `llm_turns`. `0` (or absent) = never expire.
    #[serde(default)]
    pub turns: u32,
    /// Per-granularity retention for `llm_metrics`, in days. Key = granularity
    /// label (e.g. `"10s"`, `"1m"`, `"5m"`, `"1h"`). Absent or 0 = never expire.
    #[serde(default)]
    pub metrics: HashMap<String, u32>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_interval_secs: default_retention_check_interval_secs(),
            calls: 0,
            turns: 0,
            metrics: HashMap::new(),
        }
    }
}

fn default_retention_check_interval_secs() -> u64 {
    3600
}

fn default_backend() -> String {
    "duckdb".to_string()
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
    8080
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnConfig {
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    /// Buffer-and-finalize grace window: how long a buffered terminal call
    /// waits for fan-in jitter before its turn is partitioned and emitted.
    /// See `docs/design/04b-turn-reorder-proposal.md` §6.3.
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
    1000
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
        assert_eq!(cfg.flush_interval_ms, 1000);
    }

    #[test]
    fn retention_config_disabled_by_default() {
        let cfg = RetentionConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.check_interval_secs, 3600);
        assert_eq!(cfg.calls, 0);
        assert_eq!(cfg.turns, 0);
        assert!(cfg.metrics.is_empty());
    }

    #[test]
    fn storage_config_embeds_retention_defaults() {
        let cfg = StorageConfig::default();
        assert!(!cfg.retention.enabled);
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
    fn pcap_config_with_custom_stream_id() {
        let toml = r#"
            [[capture.sources]]
            type = "pcap"
            interface = "eth0"
            stream_id = "my-stream"
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert_eq!(cfg.pipelines.len(), 1);
        match &cfg.pipelines[0].sources[0] {
            CaptureSourceConfig::Pcap { stream_id, .. } => {
                assert_eq!(stream_id.as_deref(), Some("my-stream"));
            }
            _ => panic!("expected Pcap"),
        }
    }

    #[test]
    fn resolved_stream_id_defaults() {
        let pcap = CaptureSourceConfig::Pcap {
            interface: "eth1".to_string(),
            bpf_filter: None,
            snaplen: 65535,
            stream_id: None,
        };
        assert_eq!(pcap.resolved_stream_id(), Some("eth1".to_string()));

        let pcap_file = CaptureSourceConfig::PcapFile {
            path: "/data/captures/test.pcap".to_string(),
            realtime: false,
            stream_id: None,
        };
        assert_eq!(pcap_file.resolved_stream_id(), Some("test".to_string()));

        let cloud = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
            recv_hwm: 1000,
        };
        assert_eq!(cloud.resolved_stream_id(), None);
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
}
