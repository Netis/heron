//! End-to-end: read pcap → ts-protocol stage → ts-llm stage →
//! ts-turn tracker → assert turn counts against ground truth.
//!
//! Skips gracefully if fixtures are missing (they are gitignored).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use ts_capture::{CaptureSource, PcapFileSource, RoutingSender};
use ts_common::internal_metrics::{Metric, MetricsSystem};
use ts_protocol::{spawn_flow_dispatcher, spawn_protocol_stage};
use ts_turn::tracker::TrackerConfig;
use ts_turn::TurnStatus;

fn fixture(name: &str) -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/pcaps")
        .join(name);
    if root.exists() {
        Some(root)
    } else {
        None
    }
}

async fn run_pcap_sharded(
    name: &str,
    turn_shards: usize,
    metrics_shards: usize,
) -> Option<Vec<ts_turn::LlmTurn>> {
    let path = fixture(name)?;
    let mut metrics_sys = MetricsSystem::new();

    let source_metrics = metrics_sys.register_worker(
        "capture.test",
        &[
            Metric::CapturePacketsReceived,
            Metric::CapturePacketsDropped,
        ],
    );

    let queue_size = 4096usize;
    let (raw_tx, raw_rx) = mpsc::channel::<ts_capture::RawPacket>(queue_size);
    let (parsed_tx, parsed_rx) = mpsc::channel::<ts_protocol::WorkerInput>(queue_size);
    let (event_tx, event_rx) = mpsc::channel(queue_size);

    let mut turn_shard_txs = Vec::with_capacity(turn_shards);
    let mut turn_shard_rxs = Vec::with_capacity(turn_shards);
    for _ in 0..turn_shards {
        let (tx, rx) = mpsc::channel::<ts_llm::model::TurnShardInput>(queue_size);
        turn_shard_txs.push(tx);
        turn_shard_rxs.push(rx);
    }

    let mut metrics_shard_txs = Vec::with_capacity(metrics_shards);
    let mut metrics_shard_rxs = Vec::with_capacity(metrics_shards);
    for _ in 0..metrics_shards {
        let (tx, rx) = mpsc::channel::<ts_llm::model::LlmEvent>(queue_size);
        metrics_shard_txs.push(tx);
        metrics_shard_rxs.push(rx);
    }

    let (calls_tx, mut calls_rx) = mpsc::channel::<Arc<ts_llm::model::LlmCall>>(queue_size);
    let (turns_tx, mut turns_rx) = mpsc::channel::<ts_turn::LlmTurn>(queue_size);
    let (m_out_tx, mut m_out_rx) = mpsc::channel::<ts_metrics::model::LlmMetric>(queue_size);

    spawn_flow_dispatcher(raw_rx, vec![parsed_tx], "dispatcher", &mut metrics_sys);
    spawn_protocol_stage(vec![parsed_rx], vec![event_tx], &mut metrics_sys);

    let registry = Arc::new(ts_llm::profiles::build_default_registry());
    ts_llm::spawn_llm_stage(
        vec![event_rx],
        turn_shard_txs,
        metrics_shard_txs,
        calls_tx,
        registry,
        &mut metrics_sys,
    );

    ts_turn::spawn_turn_stage(
        TrackerConfig::default(),
        turn_shard_rxs,
        turns_tx,
        Arc::new(ts_llm::profiles::build_default_registry()),
        &mut metrics_sys,
    );

    ts_metrics::spawn_metrics_stage(metrics_shard_rxs, m_out_tx, &mut metrics_sys);

    let _metrics_svc = metrics_sys.start();

    let source = PcapFileSource::new(path, "test".to_string());
    let cancel = tokio_util::sync::CancellationToken::new();
    let src_task = tokio::spawn({
        let tx = raw_tx.clone();
        let cancel = cancel.clone();
        async move {
            let _ = Box::new(source)
                .run(RoutingSender::single(tx), source_metrics, cancel)
                .await;
        }
    });
    drop(raw_tx);

    let calls_drain = tokio::spawn(async move { while calls_rx.recv().await.is_some() {} });
    let metrics_drain = tokio::spawn(async move { while m_out_rx.recv().await.is_some() {} });

    let mut finalized: Vec<ts_turn::LlmTurn> = Vec::new();
    while let Some(turn) = turns_rx.recv().await {
        finalized.push(turn);
    }

    let _ = src_task.await;
    let _ = calls_drain.await;
    let _ = metrics_drain.await;
    Some(finalized)
}

async fn run_pcap(name: &str) -> Option<Vec<ts_turn::LlmTurn>> {
    run_pcap_sharded(name, 1, 1).await
}

#[tokio::test]
async fn claude_cli_messages_expects_one_complete_turn() {
    let Some(turns) = run_pcap("claude-cli-messages.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let anthropic: Vec<_> = turns.iter().filter(|t| t.provider == "anthropic").collect();
    eprintln!("claude-cli-messages: {} anthropic turns", anthropic.len());
    for t in &anthropic {
        eprintln!(
            "  turn {} status={:?} calls={}",
            t.turn_id, t.status, t.call_count
        );
    }
    assert_eq!(
        anthropic.len(),
        1,
        "expected 1 turn; got {}",
        anthropic.len()
    );
    assert_eq!(anthropic[0].status, TurnStatus::Complete);
    assert_eq!(anthropic[0].client_kind, "claude-cli");
}

#[tokio::test]
async fn claude_cli_messages_multi_expects_two_turns() {
    let Some(turns) = run_pcap("claude-cli-messages-multi.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let anthropic: Vec<_> = turns.iter().filter(|t| t.provider == "anthropic").collect();
    eprintln!(
        "claude-cli-messages-multi: {} anthropic turns",
        anthropic.len()
    );
    for t in &anthropic {
        eprintln!(
            "  turn {} status={:?} calls={}",
            t.turn_id, t.status, t.call_count
        );
    }
    // Ground truth: 2 main-agent turns. The auto session-title-generation
    // one-shot is filtered (empty tools → auxiliary); Task sub-agent
    // invocations attach to their parent turns AND their terminal finish
    // signals do not close the parent (profile.subagent tags them; tracker
    // skips terminal state updates for sub-agent calls).
    assert_eq!(
        anthropic.len(),
        2,
        "expected 2 turns; got {}",
        anthropic.len()
    );
    assert!(anthropic.iter().all(|t| t.client_kind == "claude-cli"));
    let sessions: std::collections::BTreeSet<_> =
        anthropic.iter().map(|t| t.session_id.as_str()).collect();
    assert_eq!(sessions.len(), 1, "all turns must share one session_id");
    let statuses: std::collections::BTreeSet<_> =
        anthropic.iter().map(|t| t.status.to_string()).collect();
    assert!(
        statuses.contains("complete"),
        "expected at least one complete turn; statuses={statuses:?}"
    );
}

#[tokio::test]
async fn codex_cli_messages_multi_expects_two_turns() {
    let Some(turns) = run_pcap("codex-cli-messages-multi.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let openai: Vec<_> = turns
        .iter()
        .filter(|t| t.provider == "openai-responses")
        .collect();
    eprintln!("codex-cli-messages-multi: {} openai turns", openai.len());
    for t in &openai {
        eprintln!(
            "  turn {} status={:?} calls={}",
            t.turn_id, t.status, t.call_count
        );
    }
    assert_eq!(openai.len(), 2, "expected 2 turns; got {}", openai.len());
    assert!(openai.iter().all(|t| t.client_kind == "codex-cli"));
    let sessions: std::collections::BTreeSet<_> =
        openai.iter().map(|t| t.session_id.as_str()).collect();
    assert_eq!(sessions.len(), 1, "all turns must share one session_id");
    // Plan B: explicit-path closes a Codex turn immediately when the call's
    // response.output contains no function_call items (no further roundtrip).
    // Turn 1 (16 calls) ends with a clean assistant message → must be Complete.
    // Turn 2 (23 calls) is cut off by EOF mid-roundtrip → stays Incomplete via
    // flush_all. Without the fix, both would be Incomplete (closed only at EOF).
    let complete_count = openai
        .iter()
        .filter(|t| t.status == TurnStatus::Complete)
        .count();
    assert!(
        complete_count >= 1,
        "expected at least one Complete turn (Plan B: terminal call closes immediately); got {:?}",
        openai.iter().map(|t| t.status).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn claude_cli_messages_multi_pcap_shard_parity() {
    let Some(single) = run_pcap_sharded("claude-cli-messages-multi.pcap", 1, 1).await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let multi = run_pcap_sharded("claude-cli-messages-multi.pcap", 4, 4)
        .await
        .unwrap();

    let single_keys: std::collections::BTreeSet<_> = single
        .iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
        .collect();
    let multi_keys: std::collections::BTreeSet<_> = multi
        .iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
        .collect();
    assert_eq!(
        single_keys, multi_keys,
        "turn sets must match across shard counts"
    );
}

#[tokio::test]
async fn codex_cli_messages_multi_pcap_shard_parity() {
    let Some(single) = run_pcap_sharded("codex-cli-messages-multi.pcap", 1, 1).await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let multi = run_pcap_sharded("codex-cli-messages-multi.pcap", 4, 4)
        .await
        .unwrap();

    let single_keys: std::collections::BTreeSet<_> = single
        .iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
        .collect();
    let multi_keys: std::collections::BTreeSet<_> = multi
        .iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
        .collect();
    assert_eq!(
        single_keys, multi_keys,
        "turn sets must match across shard counts"
    );
}

#[tokio::test]
async fn claude_cli_messages_multi_shard_parity() {
    let Some(single) = run_pcap_sharded("claude-cli-messages.pcap", 1, 1).await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let multi = run_pcap_sharded("claude-cli-messages.pcap", 4, 4)
        .await
        .unwrap();

    let single_keys: std::collections::BTreeSet<_> = single
        .iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
        .collect();
    let multi_keys: std::collections::BTreeSet<_> = multi
        .iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
        .collect();
    assert_eq!(
        single_keys, multi_keys,
        "turn sets must match across shard counts"
    );
}
