//! End-to-end pipeline test.
//!
//! Drives `Pipeline::build` with a pcap fixture, waits for the EOF cascade to
//! drain every stage, then opens a fresh DuckDB connection to verify that all
//! three tables (`spans`, `traces`, `llm_metrics`) contain the
//! expected rows. Unlike the per-stage integration tests, this one exercises
//! the composition root and the storage sink together — regressions in
//! channel wiring, shard-fan-out, or EOF propagation surface here.
//!
//! Skips gracefully when the pcap fixture is absent (fixtures are gitignored).

use std::path::PathBuf;

use duckdb::Connection;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use h_capture::{CaptureSource, PcapFileSource};
use h_common::config::{
    CaptureSourceConfig, DuckDbConfig, PipelineDef, RetentionConfig, StorageConfig,
    StorageSinkConfig,
};
use h_common::internal_metrics::{Metric, MetricsSystem};
use h_llm::wire_apis as wa;
use heron::create_backend;
use heron::Pipeline;

fn fixture(name: &str) -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/pcaps")
        .join(name);
    root.exists().then_some(root)
}

fn build_storage_config(db_path: &str) -> StorageConfig {
    StorageConfig {
        backend: "duckdb".into(),
        duckdb: DuckDbConfig {
            path: db_path.into(),
        },
        sink: StorageSinkConfig::default(),
        retention: RetentionConfig::default(),
        ..Default::default()
    }
}

/// Runs the pcap fixture through the full pipeline into an on-disk DuckDB.
/// Returns the TempDir so the caller can open a verification connection
/// against the same file before it is cleaned up.
async fn run_pipeline(fixture_name: &str) -> Option<(TempDir, PathBuf)> {
    run_pipeline_multi(&[fixture_name]).await
}

/// Runs **multiple** pcap fixtures through the pipeline in parallel, one
/// capture source per fixture, all feeding the same pipeline.
/// Returns `None` if any fixture is missing (gracefully skipped).
async fn run_pipeline_multi(fixture_names: &[&str]) -> Option<(TempDir, PathBuf)> {
    let pcap_paths: Vec<PathBuf> = fixture_names
        .iter()
        .map(|n| fixture(n))
        .collect::<Option<Vec<_>>>()?;

    let tmp = tempfile::tempdir().expect("create tempdir");
    let db_path = tmp.path().join("test.duckdb");
    let storage_config = build_storage_config(&db_path.to_string_lossy());

    let storage = create_backend(&storage_config).expect("create backend");
    storage.init().await.expect("init storage");

    // One PipelineDef per pcap source — each pipeline has its own
    // dispatcher/protocol/llm/turn/metrics stages, so flow keys and turn
    // state are fully isolated (matching the old sub-pipeline-per-source
    // semantics). Only the storage sink is shared.
    let pipeline_defs: Vec<PipelineDef> = pcap_paths
        .iter()
        .enumerate()
        .map(|(i, p)| PipelineDef {
            name: format!("e2e-{i}"),
            sources: vec![CaptureSourceConfig::PcapFile {
                path: p.to_string_lossy().to_string(),
                realtime: false,
                source_id: None,
                loop_count: 1,
                loop_secs: 0,
                rate_pps: 0,
            }],
            ..PipelineDef::default()
        })
        .collect();

    // One MetricsSystem per pipeline.
    let mut per_pipeline_metrics: Vec<MetricsSystem> = (0..pipeline_defs.len())
        .map(|_| MetricsSystem::new())
        .collect();
    let mut shared_metrics = MetricsSystem::new();

    // Register capture metrics for each pipeline's single source.
    let capture_metrics: Vec<_> = per_pipeline_metrics
        .iter_mut()
        .enumerate()
        .map(|(i, sys)| {
            sys.register_worker(
                &format!("capture.e2e.{i}"),
                &[
                    Metric::CapturePacketsReceived,
                    Metric::CaptureKernelPacketsDropped,
                    Metric::CaptureTruncatedPackets,
                ],
            )
        })
        .collect();

    let sink_config = h_storage::StorageSinkConfig {
        batch_size: storage_config.sink.batch_size,
        flush_interval_ms: storage_config.sink.flush_interval_ms,
    };

    let Pipeline {
        pipeline_txs,
        pipeline_sources: _,
        stage_handles,
    } = Pipeline::build(
        &pipeline_defs,
        &sink_config,
        storage.clone(),
        &mut per_pipeline_metrics,
        &mut shared_metrics,
        h_turn::new_active_trace_registry(),
        h_llm::agent_classifier::ClassifierConfig::default(),
        h_common::config::BodyCapConfig::default(),
        h_common::attribution::AttributionConfig::default(),
    );
    let _metrics_svcs: Vec<_> = per_pipeline_metrics
        .into_iter()
        .map(|s| s.start())
        .collect();
    let _shared_metrics_svc = shared_metrics.start();

    // Each pcap source owns its pipeline's RawPacket sender; dropping it on
    // source exit cascades EOF through that pipeline.
    let mut src_tasks = Vec::new();
    for ((path, (_name, tx)), metrics) in pcap_paths
        .into_iter()
        .zip(pipeline_txs.into_iter())
        .zip(capture_metrics.into_iter())
    {
        let cancel = CancellationToken::new();
        src_tasks.push(tokio::spawn(async move {
            let source_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let source = Box::new(PcapFileSource::new(path, source_id, None));
            let _ = source.run(tx, metrics, cancel).await;
        }));
    }

    for t in src_tasks {
        t.await.expect("pcap source task panicked");
    }

    // Pcap EOFs cascade down every pipeline; once all senders are dropped,
    // the shared sink observes EOF. Awaiting every handle guarantees all
    // batches have been flushed.
    for (task, h) in stage_handles {
        h.await
            .unwrap_or_else(|e| panic!("stage '{task}' panicked: {e}"));
    }

    // Release the pipeline's Arc<dyn StorageBackend> so DuckDB's connection
    // is dropped before we open a verification connection against the same
    // file.
    drop(storage);

    Some((tmp, db_path))
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

#[tokio::test]
async fn claude_cli_pcap_populates_all_three_tables() {
    let Some((_tmp, db_path)) = run_pipeline("claude-cli-messages.pcap").await else {
        eprintln!("skip: claude-cli-messages.pcap fixture not present");
        return;
    };

    let conn = Connection::open(&db_path).expect("reopen duckdb for verify");

    let calls = count(&conn, "spans");
    let turns = count(&conn, "traces");
    let metrics = count(&conn, "llm_metrics");
    eprintln!("e2e rows: calls={calls} turns={turns} metrics={metrics}");

    assert!(calls >= 1, "expected >=1 spans, got {calls}");
    assert!(turns >= 1, "expected >=1 traces, got {turns}");
    assert!(metrics >= 1, "expected >=1 llm_metrics, got {metrics}");

    // Wire-API ground truth: fixture is an anthropic Messages API capture.
    let call_wire_apis: Vec<String> = conn
        .prepare("SELECT DISTINCT wire_api FROM spans")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        call_wire_apis.iter().any(|p| p == wa::ANTHROPIC),
        "expected anthropic in spans wire_apis, got {call_wire_apis:?}"
    );

    // A single complete claude-cli turn is the documented ground truth
    // (matches `h-turn/tests/integration.rs::claude_cli_messages_expects_one_complete_turn`).
    let (anthropic_turns, status, agent_kind): (i64, String, String) = conn
        .query_row(
            "SELECT COUNT(*), MIN(status), MIN(agent_kind) \
             FROM traces WHERE wire_api = 'anthropic'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("turn summary query");
    assert_eq!(anthropic_turns, 1, "expected exactly 1 anthropic turn");
    assert_eq!(status, "complete", "turn status should be 'complete'");
    assert_eq!(agent_kind, "claude-cli");

    // Metrics must have at least one anthropic 10s bucket with a non-zero
    // call_count — proves LLM stage → metrics shard → sink wiring end-to-end.
    let anthropic_requests_10s: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(call_count), 0) FROM llm_metrics \
             WHERE granularity = '10s' AND wire_api = 'anthropic'",
            [],
            |r| r.get(0),
        )
        .expect("metrics sum query");
    assert!(
        anthropic_requests_10s >= 1,
        "expected >=1 anthropic request in 10s metrics, got {anthropic_requests_10s}"
    );

    // A completed turn's final_call_id must reference a real row in
    // spans — catches future divergence between turn shard fan-out and
    // call sink writes.
    let dangling_final_call: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM traces t \
             WHERE t.final_call_id IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM spans c WHERE c.id = t.final_call_id)",
            [],
            |r| r.get(0),
        )
        .expect("dangling final_call_id query");
    assert_eq!(
        dangling_final_call, 0,
        "turn.final_call_id must reference an existing spans row"
    );

    // A complete anthropic turn must have call_count consistent with how
    // many anthropic calls landed in spans (≤, since some calls may
    // belong to unfinalised turns in other fixtures).
    let (turn_call_count, anthropic_calls): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT MIN(call_count) FROM traces WHERE wire_api = 'anthropic'), \
                    (SELECT COUNT(*) FROM spans WHERE wire_api = 'anthropic')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("call_count vs calls query");
    assert!(
        turn_call_count >= 1 && turn_call_count <= anthropic_calls,
        "turn.call_count ({turn_call_count}) out of range 1..={anthropic_calls}"
    );
}

/// Drives two different pcap fixtures through two capture sources
/// simultaneously and asserts that:
///
/// * Both wire APIs land in `spans` — proves each sub-pipeline reached
///   the shared sink independently.
/// * The anthropic-only fixture produces exactly 1 complete anthropic turn
///   (matches the single-source E2E's ground truth), which rules out
///   turn-state leakage from the concurrent openai-responses capture.
/// * `llm_metrics` contains rows from **both** sources (keyed by pcap file
///   basename), proving per-capture metrics stages run end-to-end and both
///   land in the shared sink.
#[tokio::test]
async fn two_pcaps_isolated_but_metrics_merged() {
    let Some((_tmp, db_path)) =
        run_pipeline_multi(&["claude-cli-messages.pcap", "codex-cli-messages-multi.pcap"]).await
    else {
        eprintln!("skip: one or both two-pcap fixtures not present");
        return;
    };

    let conn = Connection::open(&db_path).expect("reopen duckdb for verify");

    let wire_apis: Vec<String> = conn
        .prepare("SELECT DISTINCT wire_api FROM spans ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        wire_apis.iter().any(|p| p == wa::ANTHROPIC),
        "expected anthropic in spans wire_apis, got {wire_apis:?}"
    );
    assert!(
        wire_apis.iter().any(|p| p == wa::OPENAI_RESPONSES),
        "expected openai-responses in spans wire_apis, got {wire_apis:?}"
    );

    // The anthropic fixture alone produces exactly 1 complete turn. If the
    // two sub-pipelines leaked state into one another (shared turn
    // tracker, flow-key collisions, …), this count would drift.
    let anthropic_turns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM traces \
             WHERE wire_api = 'anthropic' AND status = 'complete'",
            [],
            |r| r.get(0),
        )
        .expect("anthropic turn count query");
    assert_eq!(
        anthropic_turns, 1,
        "anthropic source must still produce exactly 1 complete turn alongside codex"
    );

    // The openai-responses (codex) capture must have at least one turn too
    // — proves its sub-pipeline ran end-to-end and wasn't starved by the
    // anthropic one.
    let openai_turns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM traces WHERE wire_api = 'openai-responses'",
            [],
            |r| r.get(0),
        )
        .expect("openai turn count query");
    assert!(
        openai_turns >= 1,
        "expected >=1 openai-responses turn, got {openai_turns}"
    );

    // Per-capture metrics: both wire APIs must appear in llm_metrics and
    // each must have been emitted by its own source. Source IDs are derived
    // from the pcap file basenames.
    let metric_wire_apis: Vec<String> = conn
        .prepare("SELECT DISTINCT wire_api FROM llm_metrics ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        metric_wire_apis.iter().any(|p| p == wa::ANTHROPIC),
        "expected anthropic in llm_metrics wire_apis, got {metric_wire_apis:?}"
    );
    assert!(
        metric_wire_apis.iter().any(|p| p == wa::OPENAI_RESPONSES),
        "expected openai-responses in llm_metrics wire_apis, got {metric_wire_apis:?}"
    );

    let source_ids: Vec<String> = conn
        .prepare("SELECT DISTINCT source_id FROM llm_metrics ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        source_ids.len(),
        2,
        "expected 2 distinct source_ids in llm_metrics, got {source_ids:?}"
    );
    assert!(
        source_ids.iter().any(|s| s == "claude-cli-messages"),
        "expected 'claude-cli-messages' source_id, got {source_ids:?}"
    );
    assert!(
        source_ids.iter().any(|s| s == "codex-cli-messages-multi"),
        "expected 'codex-cli-messages-multi' source_id, got {source_ids:?}"
    );
}
