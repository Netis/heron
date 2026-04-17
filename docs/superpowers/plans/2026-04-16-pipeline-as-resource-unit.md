# Pipeline as Resource Unit — `[[pipeline]]` Config Restructure

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `pipeline` the top-level resource isolation concept: each `[[pipeline]]` owns its sources and shard counts, multiple sources share one set of workers within a pipeline, and CLI captures override all config pipelines with a single default pipeline.

**Architecture:** Replace `[pipeline]` (singleton) + `[[capture.sources]]` (flat list) with `[[pipeline]]` (array), each containing nested `[[pipeline.sources]]` and its own shard/queue config. `Pipeline::build` changes from "one sub-pipeline per source" to "one pipeline = one set of workers, N sources fan-in via a shared `raw_tx` channel." CLI `--pcap-file` / `-i` ignores all config pipelines and creates a single default-parameter pipeline. Storage sink remains globally shared across all pipelines.

**Tech Stack:** Rust, serde (TOML deserialization), Tokio mpsc, clap

---

## Context

After the stream_id refactor, all internal state (FlowKey, TurnKey, BucketKey) is namespaced by `stream_id`. Multiple capture sources can safely share a single set of worker tasks. This refactor promotes `pipeline` from an implicit singleton to an explicit, user-facing resource unit.

**Current config:**
```toml
[[capture.sources]]
type = "pcap"
interface = "eth0"

[pipeline]
flow_shard_count = 4
```

**New config:**
```toml
[[pipeline]]
name = "local"
flow_shard_count = 4

[[pipeline.sources]]
type = "pcap"
interface = "eth0"

[[pipeline.sources]]
type = "pcap"
interface = "eth1"

[[pipeline]]
name = "cloud"
flow_shard_count = 8

[[pipeline.sources]]
type = "cloud-probe"
endpoint = "tcp://0.0.0.0:5555"
```

**CLI override rule:** When `--pcap-file` or `-i` is provided, all `[[pipeline]]` entries from the config file are ignored. A single pipeline with default parameters is created containing that one CLI source.

## File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `server/ts-common/src/config.rs` | New `PipelineDef` struct, `[[pipeline]]` array, deprecate `CaptureConfig`, compat shim |
| Modify | `server/app/tokenscope/src/pipeline.rs` | `Pipeline::build` takes `&[PipelineDef]`, builds one worker set per pipeline, fan-in sources |
| Modify | `server/app/tokenscope/src/main.rs` | CLI override logic, resolve `Vec<PipelineDef>`, stream_id validation |
| Modify | `server/config/default.toml` | New `[[pipeline]]` format |
| Modify | `server/app/tokenscope/tests/pipeline_e2e.rs` | Adapt test config construction |

---

## Task 1: Config — New `PipelineDef` struct with nested sources

**Files:**
- Modify: `server/ts-common/src/config.rs`

- [ ] **Step 1: Write failing test for new config format**

```rust
#[test]
fn pipeline_array_with_nested_sources() {
    let toml = r#"
        [[pipeline]]
        name = "local"
        flow_shard_count = 8

        [[pipeline.sources]]
        type = "pcap"
        interface = "eth0"

        [[pipeline.sources]]
        type = "pcap"
        interface = "eth1"

        [[pipeline]]
        name = "cloud"

        [[pipeline.sources]]
        type = "cloud-probe"
        endpoint = "tcp://0.0.0.0:6666"
    "#;
    let cfg: AppConfig = Config::builder()
        .add_source(config::File::from_str(toml, config::FileFormat::Toml))
        .build().unwrap().try_deserialize().unwrap();
    assert_eq!(cfg.pipelines.len(), 2);
    assert_eq!(cfg.pipelines[0].name, "local");
    assert_eq!(cfg.pipelines[0].flow_shard_count, 8);
    assert_eq!(cfg.pipelines[0].sources.len(), 2);
    assert_eq!(cfg.pipelines[1].name, "cloud");
    assert_eq!(cfg.pipelines[1].flow_shard_count, 4); // default
    assert_eq!(cfg.pipelines[1].sources.len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ts-common pipeline_array_with_nested_sources`
Expected: FAIL — `pipelines` field doesn't exist on `AppConfig`

- [ ] **Step 3: Define `PipelineDef` and update `AppConfig`**

Add a new struct `PipelineDef` that bundles what was previously spread across `CaptureConfig` + `PipelineConfig`:

```rust
/// One named pipeline: a set of sources sharing a worker pool.
/// Corresponds to one `[[pipeline]]` entry in the config file.
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineDef {
    #[serde(default = "default_pipeline_name")]
    pub name: String,
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
            flow_shard_count: default_flow_shard_count(),
            queues: QueueConfig::default(),
            turn: TurnConfig::default(),
            metrics: MetricsConfig::default(),
            sources: Vec::new(),
        }
    }
}

fn default_pipeline_name() -> String {
    "default".to_string()
}
```

Update `AppConfig` to support **both** old and new formats. The `config` crate deserializes TOML `[[pipeline]]` as a `Vec<PipelineDef>` and `[pipeline]` as a single `PipelineConfig`. We use an intermediate raw struct to handle both:

```rust
/// Top-level application configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// Old format: `[[capture.sources]]` — migrated into `pipelines` at load time.
    #[serde(default)]
    pub capture: CaptureConfig,
    /// Old format: `[pipeline]` (singleton) — migrated into `pipelines` at load time.
    #[serde(default)]
    pub pipeline: PipelineConfig,
    /// New format: `[[pipeline]]` array. If non-empty, `capture` and `pipeline` are ignored.
    #[serde(default, alias = "pipeline")]
    pub pipelines: Vec<PipelineDef>,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub internal_metrics: InternalMetricsConfig,
    #[serde(default)]
    pub api: ApiConfig,
}
```

**Important:** The `config` crate cannot deserialize the same TOML key `pipeline` into both `pipeline: PipelineConfig` and `pipelines: Vec<PipelineDef>` at the same time. We need a different approach: use a **raw deserialization** step with `serde_json::Value` or a two-phase parse to detect which format is present. The simpler approach is:

1. Rename the old singleton to `pipeline_defaults` in the config file (backward compat via alias).
2. Or: Use `#[serde(flatten)]` with a custom deserializer.

The cleanest solution: the TOML key stays `pipeline`. When the TOML value is an **object** (old format `[pipeline]`), we deserialize it as a `PipelineConfig` and wrap it + `capture.sources` into a single `PipelineDef`. When it's an **array** (new format `[[pipeline]]`), we deserialize directly as `Vec<PipelineDef>`.

Implement this with a custom deserializer on `AppConfig`:

```rust
use serde::de::{self, Deserializer};

/// Raw intermediate for two-phase parsing.
#[derive(Deserialize)]
struct RawAppConfig {
    #[serde(default)]
    capture: CaptureConfig,
    #[serde(default)]
    pipeline: toml::Value,
    #[serde(default)]
    storage: StorageConfig,
    #[serde(default)]
    internal_metrics: InternalMetricsConfig,
    #[serde(default)]
    api: ApiConfig,
}
```

Actually, the `config` crate already parses into `config::Value` which we can introspect. A cleaner approach: **keep the new-format field name `pipeline` (array) and use a helper that detects old vs. new.**

After careful thought, the simplest correct approach is:

```rust
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub pipelines: Vec<PipelineDef>,
    pub storage: StorageConfig,
    pub internal_metrics: InternalMetricsConfig,
    pub api: ApiConfig,
}
```

And implement `Deserialize` manually on `AppConfig` to handle both formats, OR use a wrapper:

```rust
/// Intermediate struct for deserialization that handles both old and new formats.
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

/// Either a single `[pipeline]` object (old) or a `[[pipeline]]` array (new).
#[derive(Deserialize)]
#[serde(untagged)]
enum RawPipeline {
    Array(Vec<PipelineDef>),
    Single(PipelineConfig),
}
```

Then in `AppConfig::load()`, convert `RawAppConfig` into `AppConfig`:

```rust
impl AppConfig {
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
}

impl RawAppConfig {
    fn resolve(self) -> AppConfig {
        let pipelines = match self.pipeline {
            Some(RawPipeline::Array(defs)) => defs,
            Some(RawPipeline::Single(cfg)) => {
                // Old format: wrap the singleton pipeline + capture.sources
                let sources = self.capture.map(|c| c.sources).unwrap_or_default();
                vec![PipelineDef {
                    name: "default".to_string(),
                    flow_shard_count: cfg.flow_shard_count,
                    queues: cfg.queues,
                    turn: cfg.turn,
                    metrics: cfg.metrics,
                    sources,
                }]
            }
            None => {
                // No pipeline config at all — create default if capture sources exist
                let sources = self.capture.map(|c| c.sources).unwrap_or_default();
                if sources.is_empty() {
                    vec![]
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
```

Keep `CaptureConfig`, `PipelineConfig`, `CaptureSourceConfig` unchanged (they're used by the old-format path and by individual source construction).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ts-common pipeline_array_with_nested_sources`
Expected: PASS

- [ ] **Step 5: Write test for old-format backward compatibility**

```rust
#[test]
fn old_format_migrates_to_single_pipeline() {
    let toml = r#"
        [[capture.sources]]
        type = "pcap"
        interface = "eth0"

        [pipeline]
        flow_shard_count = 8
    "#;
    let cfg = AppConfig::from_toml(toml);
    assert_eq!(cfg.pipelines.len(), 1);
    assert_eq!(cfg.pipelines[0].name, "default");
    assert_eq!(cfg.pipelines[0].flow_shard_count, 8);
    assert_eq!(cfg.pipelines[0].sources.len(), 1);
}
```

Add a test helper `AppConfig::from_toml(s: &str) -> AppConfig` that runs the full load path from a string:

```rust
impl AppConfig {
    #[cfg(test)]
    pub fn from_toml(toml: &str) -> Self {
        let config = Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .expect("build config");
        let raw: RawAppConfig = config.try_deserialize().expect("deserialize");
        raw.resolve()
    }
}
```

- [ ] **Step 6: Run test — PASS**

Run: `cargo test -p ts-common old_format_migrates_to_single_pipeline`

- [ ] **Step 7: Write test for empty config (no pipelines)**

```rust
#[test]
fn empty_config_yields_no_pipelines() {
    let toml = "";
    let cfg = AppConfig::from_toml(toml);
    assert!(cfg.pipelines.is_empty());
}
```

- [ ] **Step 8: Run test — PASS**

Run: `cargo test -p ts-common empty_config_yields_no_pipelines`

- [ ] **Step 9: Fix all existing tests that construct `AppConfig` directly**

Every existing test in ts-common that constructs `AppConfig` with `capture:` and `pipeline:` fields must be updated to use `pipelines: vec![...]` instead. Grep for `AppConfig {` across the crate and update.

- [ ] **Step 10: Run all ts-common tests — PASS**

Run: `cargo test -p ts-common`

- [ ] **Step 11: Commit**

```bash
git add server/ts-common/src/config.rs
git commit -m "feat(ts-common): add PipelineDef with nested sources, old-format compat shim"
```

---

## Task 2: Pipeline::build — Accept `&[PipelineDef]`, fan-in sources

**Files:**
- Modify: `server/app/tokenscope/src/pipeline.rs`

- [ ] **Step 1: Update `StageTask` to use pipeline name instead of capture index**

Replace `capture: Option<usize>` with `pipeline: Option<String>`:

```rust
#[derive(Debug, Clone)]
pub struct StageTask {
    pub stage: &'static str,
    pub shard: Option<usize>,
    pub pipeline: Option<String>,
}

impl fmt::Display for StageTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref p) = self.pipeline {
            write!(f, "{p}.")?;
        }
        match self.shard {
            Some(i) => write!(f, "{}.{i}", self.stage),
            None => f.write_str(self.stage),
        }
    }
}
```

- [ ] **Step 2: Change `Pipeline` struct — one `raw_tx` per pipeline, not per source**

```rust
pub struct Pipeline {
    /// One `RawPacket` sender per pipeline. All sources within that pipeline
    /// fan-in to this single sender. The dispatcher reads from the matching
    /// receiver and distributes by flow key.
    pub pipeline_txs: Vec<(String, mpsc::Sender<RawPacket>)>,
    /// Source configs per pipeline, in the same order as `pipeline_txs`.
    /// Used by main.rs to build and spawn capture source tasks.
    pub pipeline_sources: Vec<Vec<CaptureSourceConfig>>,
    pub stage_handles: Vec<(StageTask, JoinHandle<()>)>,
}
```

- [ ] **Step 3: Rewrite `Pipeline::build` to iterate `&[PipelineDef]`**

The signature changes from:

```rust
pub fn build(
    config: &AppConfig,
    storage: Arc<dyn StorageBackend>,
    per_capture_metrics: &mut [MetricsSystem],
) -> Self
```

to:

```rust
pub fn build(
    pipeline_defs: &[PipelineDef],
    storage_config: &StorageSinkConfig,
    storage: Arc<dyn StorageBackend>,
    per_pipeline_metrics: &mut [MetricsSystem],
) -> Self
```

Each iteration of the loop:
- Reads shard counts and queue config from `pipeline_defs[i]`
- Creates **one** `(raw_tx, raw_rx)` channel per pipeline (not per source)
- Builds one set of workers: dispatcher, protocol, llm, turn, metrics
- Uses `pipeline_defs[i].name` as the pipeline label in `StageTask`
- Clones into shared sink senders (`calls_tx`, `turns_tx`, `metrics_out_tx`)

The key conceptual change: previously each source got its own `raw_tx`; now each **pipeline** gets one `raw_tx`, and all sources within that pipeline share it.

```rust
pub fn build(
    pipeline_defs: &[PipelineDef],
    storage_config: &ts_storage::StorageSinkConfig,
    storage: Arc<dyn StorageBackend>,
    per_pipeline_metrics: &mut [MetricsSystem],
) -> Self {
    assert_eq!(pipeline_defs.len(), per_pipeline_metrics.len());

    // Shared sink channels (all pipelines fan-in here).
    // Use the max of all pipelines' queue configs for sink capacity,
    // since they're shared. Default to 4096 if no pipelines.
    let sink_capacity = pipeline_defs.iter()
        .map(|d| d.queues.call_sink.max(d.queues.turn_sink).max(d.queues.metric_sink))
        .max()
        .unwrap_or(4096);
    let (calls_tx, calls_rx) = mpsc::channel::<Arc<LlmCall>>(sink_capacity);
    let (turns_tx, turns_rx) = mpsc::channel::<LlmTurn>(sink_capacity);
    let (metrics_out_tx, metrics_out_rx) = mpsc::channel::<LlmMetric>(sink_capacity);

    let registry = Arc::new(ts_llm::profiles::build_default_registry());

    let mut stage_handles: Vec<(StageTask, JoinHandle<()>)> = Vec::new();
    let mut pipeline_txs: Vec<(String, mpsc::Sender<RawPacket>)> = Vec::with_capacity(pipeline_defs.len());
    let mut pipeline_sources: Vec<Vec<CaptureSourceConfig>> = Vec::with_capacity(pipeline_defs.len());

    for (i, (def, metrics_sys)) in pipeline_defs.iter().zip(per_pipeline_metrics.iter_mut()).enumerate() {
        let name = &def.name;
        let q = &def.queues;

        // One raw channel per pipeline — all sources fan-in here.
        let (raw_tx, raw_rx) = mpsc::channel::<RawPacket>(q.raw);

        let (parsed_txs, parsed_rxs) =
            make_shard_channels::<WorkerInput>(def.flow_shard_count, q.parsed_packet);
        let (event_txs, event_rxs) =
            make_shard_channels::<ts_protocol::model::ProtocolEvent>(def.flow_shard_count, q.flow_event);
        let (turn_shard_txs, turn_shard_rxs) =
            make_shard_channels::<TurnShardInput>(def.turn.shard_count, q.turn_event);
        let (metrics_shard_txs, metrics_shard_rxs) =
            make_shard_channels::<LlmEvent>(def.metrics.shard_count, q.metrics_event);

        let tracker_cfg = TrackerConfig {
            idle_timeout_us: (def.turn.idle_timeout_secs as i64) * 1_000_000,
            sweep_interval_us: (def.turn.sweep_interval_secs as i64) * 1_000_000,
        };

        // Dispatcher
        let dispatcher_handle = spawn_flow_dispatcher(raw_rx, parsed_txs, metrics_sys);
        stage_handles.push((StageTask { stage: "dispatcher", shard: None, pipeline: Some(name.clone()) }, dispatcher_handle));

        // Protocol
        let protocol_handles = spawn_protocol_stage(parsed_rxs, event_txs, metrics_sys);
        for (j, h) in protocol_handles.into_iter().enumerate() {
            stage_handles.push((StageTask { stage: "protocol", shard: Some(j), pipeline: Some(name.clone()) }, h));
        }

        // LLM
        let llm_handles = ts_llm::spawn_llm_stage(event_rxs, turn_shard_txs, metrics_shard_txs, calls_tx.clone(), registry.clone());
        for (j, h) in llm_handles.into_iter().enumerate() {
            stage_handles.push((StageTask { stage: "llm", shard: Some(j), pipeline: Some(name.clone()) }, h));
        }

        // Turn
        let turn_handles = ts_turn::spawn_turn_stage(tracker_cfg, turn_shard_rxs, turns_tx.clone(), registry.clone(), metrics_sys);
        for (j, h) in turn_handles.into_iter().enumerate() {
            stage_handles.push((StageTask { stage: "turn", shard: Some(j), pipeline: Some(name.clone()) }, h));
        }

        // Metrics
        let metrics_handles = ts_metrics::spawn_metrics_stage(metrics_shard_rxs, metrics_out_tx.clone(), metrics_sys);
        for (j, h) in metrics_handles.into_iter().enumerate() {
            stage_handles.push((StageTask { stage: "metrics", shard: Some(j), pipeline: Some(name.clone()) }, h));
        }

        pipeline_txs.push((name.clone(), raw_tx));
        pipeline_sources.push(def.sources.clone());
    }

    drop(calls_tx);
    drop(turns_tx);
    drop(metrics_out_tx);

    // Shared storage sink
    let sink_handle = ts_storage::spawn_storage_sink_stage(
        ts_storage::StorageSinkConfig {
            batch_size: storage_config.batch_size,
            flush_interval_ms: storage_config.flush_interval_ms,
        },
        calls_rx, turns_rx, metrics_out_rx, storage,
    );
    stage_handles.push((StageTask { stage: "storage_sink", shard: None, pipeline: None }, sink_handle));

    Pipeline { pipeline_txs, pipeline_sources, stage_handles }
}
```

- [ ] **Step 4: Fix compilation — update `supervise` and any other methods**

`supervise` stays unchanged (it takes `Vec<(StageTask, JoinHandle<()>)>`). But `StageTask` no longer implements `Copy` (it has a `String`), so check for any `Copy` usage.

- [ ] **Step 5: Run cargo check**

Run: `cargo check -p tokenscope`
Expected: PASS (compilation only — main.rs and tests need updating next)

- [ ] **Step 6: Commit**

```bash
git add server/app/tokenscope/src/pipeline.rs server/app/tokenscope/src/lib.rs
git commit -m "refactor(pipeline): Pipeline::build takes &[PipelineDef], one worker set per pipeline"
```

---

## Task 3: main.rs — CLI override, pipeline resolution, source spawning

**Files:**
- Modify: `server/app/tokenscope/src/main.rs`

- [ ] **Step 1: Implement pipeline resolution with CLI override**

Replace the source_configs assembly block (lines ~125-155) and the `Pipeline::build` call:

```rust
use ts_common::config::PipelineDef;

// ---- Resolve effective pipelines ----
// CLI override: --pcap-file or -i replaces ALL config pipelines with one default pipeline.
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
            heartbeat_interval_ms: 1000,
            stream_id: None,
        }],
        ..PipelineDef::default()
    }]
} else {
    config.pipelines.clone()
};
```

- [ ] **Step 2: Update stream_id validation to iterate pipelines**

```rust
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
```

- [ ] **Step 3: Update logging to show pipeline structure**

```rust
tracing::info!("  pipelines: {} configured", effective_pipelines.len());
for (i, def) in effective_pipelines.iter().enumerate() {
    tracing::info!(
        "    pipeline[{i}] '{}': flow_shards={} turn_shards={} metrics_shards={} sources={}",
        def.name, def.flow_shard_count, def.turn.shard_count, def.metrics.shard_count, def.sources.len()
    );
}
```

- [ ] **Step 4: Update MetricsSystem and Pipeline::build call**

Change from one-MetricsSystem-per-source to one-per-pipeline:

```rust
if !effective_pipelines.is_empty() && effective_pipelines.iter().any(|d| !d.sources.is_empty()) {
    let mut per_pipeline_metrics: Vec<MetricsSystem> =
        (0..effective_pipelines.len()).map(|_| MetricsSystem::new()).collect();

    let Pipeline {
        pipeline_txs,
        pipeline_sources,
        stage_handles,
    } = Pipeline::build(
        &effective_pipelines,
        &config.storage.sink,
        storage.clone(),
        &mut per_pipeline_metrics,
    );
    // ...
```

- [ ] **Step 5: Update source spawning — fan-in N sources per pipeline**

```rust
    let mut capture_tasks: JoinSet<()> = JoinSet::new();
    for ((pipeline_name, raw_tx), source_configs) in pipeline_txs.into_iter().zip(pipeline_sources.into_iter()) {
        for (j, source_cfg) in source_configs.iter().enumerate() {
            let source = match ts_capture::build_source(source_cfg) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to build source [{j}] in pipeline '{pipeline_name}': {e}");
                    std::process::exit(1);
                }
            };
            // Register per-source capture metrics against the pipeline's MetricsSystem.
            // (This requires the per_pipeline_metrics to still be accessible — register
            // before calling Pipeline::build, or pass through Pipeline.)
            let tx = raw_tx.clone();
            let cancel = cancel.clone();
            let pname = pipeline_name.clone();
            capture_tasks.spawn(async move {
                if let Err(e) = source.run(tx, /* metrics */, cancel).await {
                    tracing::error!("capture source [{j}] in pipeline '{pname}' error: {e}");
                }
            });
        }
        // Drop our clone of raw_tx — the spawned tasks hold theirs.
        // When all sources in this pipeline finish, raw_tx drops, EOF cascades.
    }
```

Note: The capture `Metric` handles need to be registered before spawning. Build sources + register metrics in a pre-pass, then spawn them. This matches the existing pattern but per-pipeline instead of per-source.

- [ ] **Step 6: Update reporter startup — per-pipeline instead of per-capture**

```rust
    let _reporter_handles: Vec<_> = per_pipeline_metrics
        .into_iter()
        .zip(effective_pipelines.iter())
        .filter_map(|(sys, def)| {
            let svc = sys.start();
            (config.internal_metrics.enabled && config.internal_metrics.interval_secs > 0).then(|| {
                let label = format!("pipeline.{}", def.name);
                MetricsReporter::start(svc, &label, Duration::from_secs(config.internal_metrics.interval_secs))
            })
        })
        .collect();
```

- [ ] **Step 7: Update the no-sources branch**

```rust
    } else {
        tracing::info!(
            "no pipelines with sources configured (use --pcap-file, -i, or [[pipeline]] in config)"
        );
        // ... rest unchanged ...
    }
```

- [ ] **Step 8: Run cargo check**

Run: `cargo check -p tokenscope`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add server/app/tokenscope/src/main.rs
git commit -m "feat(main): CLI override ignores config pipelines, per-pipeline source fan-in"
```

---

## Task 4: Update default.toml to new format

**Files:**
- Modify: `server/config/default.toml`

- [ ] **Step 1: Rewrite config file**

```toml
# TokenScope default configuration

# ---------------------------------------------------------------------------
# Pipelines
#
# Each [[pipeline]] is an independent processing pipeline with its own worker
# pool. Sources nested under a pipeline share its workers. Multiple pipelines
# provide resource isolation (e.g., separate shard counts for local capture
# vs. cloud-probe ingestion).
#
# CLI flags (--pcap-file, -i) OVERRIDE all config pipelines: when present,
# a single "cli" pipeline with default parameters is used.
# ---------------------------------------------------------------------------
# [[pipeline]]
# name = "local"
# flow_shard_count = 4
#
# [pipeline.turn]
# idle_timeout_secs = 600
# sweep_interval_secs = 10
# shard_count = 1
#
# [pipeline.metrics]
# shard_count = 1
#
# [[pipeline.sources]]
# type = "pcap"
# interface = "eth0"
# bpf_filter = "tcp port 8080"
# snaplen = 65535
# heartbeat_interval_ms = 1000
#
# [[pipeline.sources]]
# type = "cloud-probe"
# endpoint = "tcp://0.0.0.0:5555"
# recv_hwm = 1000
#
# [[pipeline.sources]]
# type = "pcap-file"
# path = "/data/captures/llm-traffic.pcap"
# realtime = false

# Bounded channel capacities between pipeline stages. All default to 4096.
# Override per-pipeline under [[pipeline]].
# [pipeline.queues]
# raw = 4096
# parsed_packet = 4096
# flow_event = 4096
# turn_event = 4096
# metrics_event = 4096
# call_sink = 4096
# turn_sink = 4096
# metric_sink = 4096

# ---------------------------------------------------------------------------
# Storage
# ---------------------------------------------------------------------------
[storage]
backend = "duckdb"

[storage.duckdb]
path = "data/tokenscope.duckdb"

# Storage sink batching
[storage.sink]
batch_size = 1000
flush_interval_ms = 1000

# Data retention (disabled by default).
# [storage.retention]
# enabled = false
# check_interval_secs = 3600
# calls = 7
# turns = 30
# [storage.retention.metrics]
# "10s" = 1
# "1m"  = 7
# "5m"  = 30
# "1h"  = 365

# ---------------------------------------------------------------------------
# Internal metrics (pipeline self-monitoring)
# ---------------------------------------------------------------------------
[internal_metrics]
enabled = true
interval_secs = 10

# ---------------------------------------------------------------------------
# API server
# ---------------------------------------------------------------------------
[api]
listen = "0.0.0.0"
port = 3000
```

- [ ] **Step 2: Verify config loads**

Run: `cargo run -- -c server/config/default.toml --help` (just parse, no capture needed)

- [ ] **Step 3: Commit**

```bash
git add server/config/default.toml
git commit -m "docs(config): update default.toml to [[pipeline]] format"
```

---

## Task 5: E2E tests — Adapt to new config shape

**Files:**
- Modify: `server/app/tokenscope/tests/pipeline_e2e.rs`

- [ ] **Step 1: Update `build_test_config` to use `pipelines`**

```rust
fn build_test_config(db_path: &str) -> AppConfig {
    AppConfig {
        pipelines: vec![],  // E2E tests add sources dynamically
        storage: StorageConfig {
            backend: "duckdb".into(),
            duckdb: DuckDbConfig { path: db_path.into() },
            sink: StorageSinkConfig::default(),
            retention: RetentionConfig::default(),
        },
        internal_metrics: InternalMetricsConfig { enabled: false, interval_secs: 0 },
        api: ApiConfig::default(),
    }
}
```

- [ ] **Step 2: Update `run_pipeline_multi` to build `PipelineDef`**

The test now creates a single `PipelineDef` containing all pcap fixtures as sources:

```rust
async fn run_pipeline_multi(fixture_names: &[&str]) -> Option<(TempDir, PathBuf)> {
    let pcap_paths: Vec<PathBuf> = fixture_names
        .iter()
        .map(|n| fixture(n))
        .collect::<Option<Vec<_>>>()?;

    let tmp = tempfile::tempdir().expect("create tempdir");
    let db_path = tmp.path().join("test.duckdb");
    let config = build_test_config(&db_path.to_string_lossy());

    let storage = create_backend(&config.storage).expect("create backend");
    storage.init().await.expect("init storage");

    // Build one PipelineDef with all pcap files as sources.
    let sources: Vec<CaptureSourceConfig> = pcap_paths.iter()
        .map(|p| CaptureSourceConfig::PcapFile {
            path: p.to_string_lossy().to_string(),
            realtime: false,
            stream_id: None,
        })
        .collect();
    let pipeline_def = PipelineDef {
        name: "e2e".to_string(),
        sources,
        ..PipelineDef::default()
    };

    let mut per_pipeline_metrics = vec![MetricsSystem::new()];

    let Pipeline {
        pipeline_txs,
        pipeline_sources,
        stage_handles,
    } = Pipeline::build(
        &[pipeline_def],
        &config.storage.sink,
        storage.clone(),
        &mut per_pipeline_metrics,
    );
    let _metrics_svcs: Vec<_> = per_pipeline_metrics.into_iter().map(|s| s.start()).collect();

    // Spawn sources — all fan-in to the single pipeline's raw_tx.
    let mut src_tasks = Vec::new();
    for ((_, raw_tx), source_cfgs) in pipeline_txs.into_iter().zip(pipeline_sources.into_iter()) {
        for source_cfg in &source_cfgs {
            let source = ts_capture::build_source(source_cfg).expect("build source");
            let tx = raw_tx.clone();
            let cancel = CancellationToken::new();
            src_tasks.push(tokio::spawn(async move {
                let _ = source.run(tx, /* metrics handle */, cancel).await;
            }));
        }
    }

    for t in src_tasks {
        t.await.expect("pcap source task panicked");
    }

    for (task, h) in stage_handles {
        h.await.unwrap_or_else(|e| panic!("stage '{task}' panicked: {e}"));
    }

    drop(storage);
    Some((tmp, db_path))
}
```

Note: The capture `Metric` handle needs to be plumbed through. The E2E test currently creates per-source metric handles. With the new model, each pipeline has one `MetricsSystem`, and each source within it registers a worker. This is handled by either:
- Registering capture workers in `Pipeline::build` (if sources are passed through), or
- Registering them in main.rs / tests before spawning sources.

The cleanest path: `Pipeline` returns the source configs so the caller can build+spawn sources and register capture metrics against the pipeline's MetricsSystem. This is what the `pipeline_sources` field achieves.

- [ ] **Step 3: Update assertions in `two_pcaps_isolated_but_metrics_merged`**

The two pcap files are now in the **same pipeline** (single set of workers) instead of two sub-pipelines. The assertions about stream_ids and providers should still hold because stream_id namespacing keeps everything separate.

- [ ] **Step 4: Run E2E tests**

Run: `cargo test -p tokenscope --test pipeline_e2e`
Expected: PASS (or skip if fixtures absent)

- [ ] **Step 5: Commit**

```bash
git add server/app/tokenscope/tests/pipeline_e2e.rs
git commit -m "test(e2e): adapt pipeline_e2e to PipelineDef-based Pipeline::build"
```

---

## Task 6: Integration test — Update ts-turn integration test

**Files:**
- Modify: `server/ts-turn/tests/integration.rs`

- [ ] **Step 1: Update to use new Pipeline::build signature if applicable**

The ts-turn integration test may construct `AppConfig` or call `Pipeline::build` directly. Update to match new config shape and `Pipeline::build` signature.

If it only calls stage-level functions (`spawn_turn_stage`, `spawn_metrics_stage`, etc.), it may not need changes — verify by checking compilation.

- [ ] **Step 2: Run tests**

Run: `cargo test -p ts-turn --test integration`
Expected: PASS

- [ ] **Step 3: Commit (if changes needed)**

```bash
git add server/ts-turn/tests/integration.rs
git commit -m "test(ts-turn): adapt integration test to new config shape"
```

---

## Task 7: Workspace-wide compilation and test pass

- [ ] **Step 1: Check entire workspace compiles**

Run: `cargo check --workspace`
Expected: PASS

- [ ] **Step 2: Run all workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass

- [ ] **Step 3: Final commit for any stragglers**

```bash
git add -u
git commit -m "fix: resolve remaining compilation issues from pipeline restructure"
```

---

## Verification

1. **Old-format config:** Place the old `default.toml` (with `[pipeline]` + `[[capture.sources]]`) and verify it loads correctly, producing a single default pipeline.
2. **New-format config:** Write a TOML with two `[[pipeline]]` entries, each with different shard counts and sources. Verify both pipelines start and process independently.
3. **CLI override:** Run `tokenscope --pcap-file test.pcap` with a config that has `[[pipeline]]` entries. Verify only the CLI source runs, config pipelines are ignored.
4. **Duplicate stream_id:** Configure two sources with the same stream_id across different pipelines. Verify startup fails with a clear error.
5. **E2E:** `cargo test -p tokenscope --test pipeline_e2e` — stream_id values appear correctly in all three tables.
