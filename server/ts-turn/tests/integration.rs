//! End-to-end: read pcap → ts-protocol stage → ts-llm stage →
//! ts-turn tracker → assert turn counts against ground truth.
//!
//! Skips gracefully if fixtures are missing (they are gitignored).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use ts_capture::{CaptureSource, PcapFileSource, RoutingSender};
use ts_common::internal_metrics::{Metric, MetricsSystem};
use ts_llm::wire_apis as wa;
use ts_protocol::{spawn_flow_dispatcher, spawn_http_joiner_stage, spawn_protocol_stage};
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

async fn run_pcap_full_sharded(
    name: &str,
    flow_shards: usize,
    turn_shards: usize,
    metrics_shards: usize,
) -> Option<Vec<ts_turn::AgentTurn>> {
    let path = fixture(name)?;
    let mut metrics_sys = MetricsSystem::new();

    let source_metrics = metrics_sys.register_worker(
        "capture.test",
        &[
            Metric::CapturePacketsReceived,
            Metric::CaptureKernelPacketsDropped,
        ],
    );

    let queue_size = 4096usize;
    let (raw_tx, raw_rx) = mpsc::channel::<ts_capture::RawPacket>(queue_size);

    // Per-flow-shard parsed/event channels. The dispatcher hashes by 5-tuple
    // and routes to a single parsed_tx; the protocol+llm stages each spawn
    // one worker per shard. Same-session calls riding on different TCP
    // connections land on different llm workers and reach the turn shard
    // out of order — exactly the reorder scenario buffer-and-finalize fixes.
    let mut parsed_txs = Vec::with_capacity(flow_shards);
    let mut parsed_rxs = Vec::with_capacity(flow_shards);
    let mut protocol_event_txs = Vec::with_capacity(flow_shards);
    let mut protocol_event_rxs = Vec::with_capacity(flow_shards);
    let mut joiner_event_txs = Vec::with_capacity(flow_shards);
    let mut joiner_event_rxs = Vec::with_capacity(flow_shards);
    for _ in 0..flow_shards {
        let (ptx, prx) = mpsc::channel::<ts_protocol::WorkerInput>(queue_size);
        parsed_txs.push(ptx);
        parsed_rxs.push(prx);
        let (etx, erx) = mpsc::channel::<ts_protocol::model::HttpParseEvent>(queue_size);
        protocol_event_txs.push(etx);
        protocol_event_rxs.push(erx);
        let (jtx, jrx) = mpsc::channel::<ts_protocol::HttpJoinerEvent>(queue_size);
        joiner_event_txs.push(jtx);
        joiner_event_rxs.push(jrx);
    }

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
    let (turns_tx, mut turns_rx) = mpsc::channel::<ts_turn::AgentTurn>(queue_size);
    let (m_out_tx, mut m_out_rx) = mpsc::channel::<ts_metrics::model::LlmMetricsBatch>(queue_size);

    spawn_flow_dispatcher(raw_rx, parsed_txs, "dispatcher", &mut metrics_sys);
    spawn_protocol_stage(parsed_rxs, protocol_event_txs, &mut metrics_sys);
    // Integration test doesn't assert on http_exchanges — pass None for the
    // storage-bound channel.
    spawn_http_joiner_stage(protocol_event_rxs, joiner_event_txs, None, &mut metrics_sys);

    let registry = Arc::new(ts_llm::agents::build_default_registry());
    let wire_api_registry = Arc::new(ts_llm::wire_apis::build_default_wire_api_registry());
    ts_llm::spawn_llm_stage(
        joiner_event_rxs,
        turn_shard_txs,
        metrics_shard_txs,
        calls_tx,
        wire_api_registry.clone(),
        registry,
        &mut metrics_sys,
    );

    ts_turn::spawn_turn_stage(
        TrackerConfig::default(),
        turn_shard_rxs,
        turns_tx,
        &mut metrics_sys,
    );

    ts_metrics::spawn_metrics_stage(metrics_shard_rxs, m_out_tx, &mut metrics_sys);

    let _metrics_svc = metrics_sys.start();

    let source = PcapFileSource::new(path, "test".to_string(), None);
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

    let mut finalized: Vec<ts_turn::AgentTurn> = Vec::new();
    while let Some(turn) = turns_rx.recv().await {
        finalized.push(turn);
    }

    let _ = src_task.await;
    let _ = calls_drain.await;
    let _ = metrics_drain.await;
    Some(finalized)
}

async fn run_pcap_sharded(
    name: &str,
    turn_shards: usize,
    metrics_shards: usize,
) -> Option<Vec<ts_turn::AgentTurn>> {
    run_pcap_full_sharded(name, 1, turn_shards, metrics_shards).await
}

async fn run_pcap(name: &str) -> Option<Vec<ts_turn::AgentTurn>> {
    run_pcap_sharded(name, 1, 1).await
}

/// Variant of `run_pcap_full_sharded` that retains every `LlmCall` instead of
/// draining the channel. Used by tests that need to inspect reconstructed
/// SSE bodies (e.g. asserting `tool_use.input` survived index-keyed
/// accumulation).
async fn run_pcap_collecting_calls(
    name: &str,
) -> Option<(Vec<ts_turn::AgentTurn>, Vec<Arc<ts_llm::model::LlmCall>>)> {
    let path = fixture(name)?;
    let mut metrics_sys = MetricsSystem::new();

    let source_metrics = metrics_sys.register_worker(
        "capture.test",
        &[
            Metric::CapturePacketsReceived,
            Metric::CaptureKernelPacketsDropped,
        ],
    );

    let queue_size = 4096usize;
    let (raw_tx, raw_rx) = mpsc::channel::<ts_capture::RawPacket>(queue_size);

    let flow_shards = 1usize;
    let turn_shards = 1usize;
    let metrics_shards = 1usize;
    let mut parsed_txs = Vec::with_capacity(flow_shards);
    let mut parsed_rxs = Vec::with_capacity(flow_shards);
    let mut protocol_event_txs = Vec::with_capacity(flow_shards);
    let mut protocol_event_rxs = Vec::with_capacity(flow_shards);
    let mut joiner_event_txs = Vec::with_capacity(flow_shards);
    let mut joiner_event_rxs = Vec::with_capacity(flow_shards);
    for _ in 0..flow_shards {
        let (ptx, prx) = mpsc::channel::<ts_protocol::WorkerInput>(queue_size);
        parsed_txs.push(ptx);
        parsed_rxs.push(prx);
        let (etx, erx) = mpsc::channel::<ts_protocol::model::HttpParseEvent>(queue_size);
        protocol_event_txs.push(etx);
        protocol_event_rxs.push(erx);
        let (jtx, jrx) = mpsc::channel::<ts_protocol::HttpJoinerEvent>(queue_size);
        joiner_event_txs.push(jtx);
        joiner_event_rxs.push(jrx);
    }

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
    let (turns_tx, mut turns_rx) = mpsc::channel::<ts_turn::AgentTurn>(queue_size);
    let (m_out_tx, mut m_out_rx) = mpsc::channel::<ts_metrics::model::LlmMetricsBatch>(queue_size);

    spawn_flow_dispatcher(raw_rx, parsed_txs, "dispatcher", &mut metrics_sys);
    spawn_protocol_stage(parsed_rxs, protocol_event_txs, &mut metrics_sys);
    spawn_http_joiner_stage(protocol_event_rxs, joiner_event_txs, None, &mut metrics_sys);

    let registry = Arc::new(ts_llm::agents::build_default_registry());
    let wire_api_registry = Arc::new(ts_llm::wire_apis::build_default_wire_api_registry());
    ts_llm::spawn_llm_stage(
        joiner_event_rxs,
        turn_shard_txs,
        metrics_shard_txs,
        calls_tx,
        wire_api_registry.clone(),
        registry,
        &mut metrics_sys,
    );

    ts_turn::spawn_turn_stage(
        TrackerConfig::default(),
        turn_shard_rxs,
        turns_tx,
        &mut metrics_sys,
    );

    ts_metrics::spawn_metrics_stage(metrics_shard_rxs, m_out_tx, &mut metrics_sys);

    let _metrics_svc = metrics_sys.start();

    let source = PcapFileSource::new(path, "test".to_string(), None);
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

    let calls_collector = tokio::spawn(async move {
        let mut acc: Vec<Arc<ts_llm::model::LlmCall>> = Vec::new();
        while let Some(c) = calls_rx.recv().await {
            acc.push(c);
        }
        acc
    });
    let metrics_drain = tokio::spawn(async move { while m_out_rx.recv().await.is_some() {} });

    let mut finalized: Vec<ts_turn::AgentTurn> = Vec::new();
    while let Some(turn) = turns_rx.recv().await {
        finalized.push(turn);
    }

    let _ = src_task.await;
    let calls = calls_collector.await.unwrap_or_default();
    let _ = metrics_drain.await;
    Some((finalized, calls))
}

#[tokio::test]
async fn claude_cli_messages_expects_one_complete_turn() {
    let Some(turns) = run_pcap("claude-cli-messages.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let anthropic: Vec<_> = turns
        .iter()
        .filter(|t| t.wire_api == wa::ANTHROPIC)
        .collect();
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
    assert_eq!(anthropic[0].agent_kind, "claude-cli");
}

#[tokio::test]
async fn claude_cli_messages_multi_expects_two_turns() {
    let Some(turns) = run_pcap("claude-cli-messages-multi.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let anthropic: Vec<_> = turns
        .iter()
        .filter(|t| t.wire_api == wa::ANTHROPIC)
        .collect();
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
    assert!(anthropic.iter().all(|t| t.agent_kind == "claude-cli"));
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
        .filter(|t| t.wire_api == wa::OPENAI_RESPONSES)
        .collect();
    eprintln!("codex-cli-messages-multi: {} openai turns", openai.len());
    for t in &openai {
        eprintln!(
            "  turn {} status={:?} calls={}",
            t.turn_id, t.status, t.call_count
        );
    }
    assert_eq!(openai.len(), 2, "expected 2 turns; got {}", openai.len());
    assert!(openai.iter().all(|t| t.agent_kind == "codex-cli"));
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

/// End-to-end reorder validation. Runs codex-cli-messages-multi.pcap
/// through the pipeline at flow_shards ∈ {1, 2, 4, 8}, holding turn_shards
/// at 1 so all calls converge into a single tracker. Higher flow_shards
/// fan the same session's calls (across multiple TCP connections) onto
/// independent llm workers, which feed the turn shard out of order — the
/// canonical scenario the buffer-and-finalize design is meant to handle.
///
/// Asserts: every configuration produces exactly 2 codex turns with the
/// same (session_id, call_count, status) tuples.
#[tokio::test]
async fn codex_cli_messages_multi_flow_shard_reorder_parity() {
    let Some(baseline) = run_pcap_full_sharded("codex-cli-messages-multi.pcap", 1, 1, 1).await
    else {
        eprintln!("skip: fixture not present");
        return;
    };
    let baseline_openai: Vec<_> = baseline
        .iter()
        .filter(|t| t.wire_api == wa::OPENAI_RESPONSES)
        .collect();
    assert_eq!(
        baseline_openai.len(),
        2,
        "baseline (flow=1) must yield 2 codex turns, got {}",
        baseline_openai.len()
    );
    let baseline_keys: std::collections::BTreeSet<_> = baseline_openai
        .iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
        .collect();
    eprintln!("flow_shards=1 baseline: {baseline_keys:?}");

    for flow_shards in [2usize, 4, 8] {
        let turns = run_pcap_full_sharded("codex-cli-messages-multi.pcap", flow_shards, 1, 1)
            .await
            .expect("fixture present");
        let openai: Vec<_> = turns
            .iter()
            .filter(|t| t.wire_api == wa::OPENAI_RESPONSES)
            .collect();
        let keys: std::collections::BTreeSet<_> = openai
            .iter()
            .map(|t| (t.session_id.clone(), t.call_count, t.status.to_string()))
            .collect();
        eprintln!("flow_shards={flow_shards}: {keys:?}");
        assert_eq!(
            openai.len(),
            2,
            "flow_shards={flow_shards} must still yield 2 codex turns, got {}",
            openai.len()
        );
        assert_eq!(
            keys, baseline_keys,
            "flow_shards={flow_shards}: turn (session, call_count, status) set must match baseline"
        );
    }
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

/// OpenClaw (OpenAI/JS SDK + GLM model) capture spanning two distinct user
/// sessions. The client reflects `assistant.tool_calls[].id` back into
/// subsequent `messages` history *without* the underscore (`calld9c1...`
/// instead of `call_d9c1...`). Without `canonicalize_tool_id`, every call #2+
/// would synth a fresh session_id and fragment each conversation into
/// single-call turns. The fact that we observe exactly 2 stable session_ids
/// each spanning multiple turns is end-to-end proof the canonicalization rule
/// is firing on this fixture.
#[tokio::test]
async fn openclaw_multi_sessions_expects_two_sessions_four_turns() {
    let Some(turns) = run_pcap("openclaw-openai.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let chat: Vec<_> = turns
        .iter()
        .filter(|t| t.wire_api == wa::OPENAI_CHAT)
        .collect();
    eprintln!("openclaw-openai: {} openai-chat turns", chat.len());
    for t in &chat {
        eprintln!(
            "  session={} status={:?} calls={}",
            t.session_id, t.status, t.call_count
        );
    }
    assert_eq!(chat.len(), 4, "expected 4 turns; got {}", chat.len());
    assert!(chat.iter().all(|t| t.agent_kind == "openclaw"));
    assert!(
        chat.iter().all(|t| t.status == TurnStatus::Complete),
        "all turns expected Complete"
    );
    let sessions: std::collections::BTreeSet<_> =
        chat.iter().map(|t| t.session_id.as_str()).collect();
    assert_eq!(
        sessions.len(),
        2,
        "expected 2 distinct sessions; got {sessions:?}"
    );
    assert!(
        sessions.iter().all(|s| s.starts_with("call_")),
        "session_ids must be canonicalized (call_<hex>); got {sessions:?}"
    );
}

#[tokio::test]
async fn openclaw_multi_sessions_pcap_shard_parity() {
    let Some(single) = run_pcap_sharded("openclaw-openai.pcap", 1, 1).await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let multi = run_pcap_sharded("openclaw-openai.pcap", 4, 4)
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

/// OpenClaw (Anthropic/JS SDK + GLM-5) capture covering one user conversation
/// plus compaction runs. GLM-5 emits parallel `tool_use` blocks where every
/// `content_block_start` arrives before any `input_json_delta`, and per-index
/// deltas/stops are interleaved in arbitrary order.
///
/// Asserts:
///   1. Pipeline produces the expected turn / session shape under the
///      `openclaw` profile: 1 session, 4 turns, all `agent_kind == "openclaw"`
///      Complete. (Compaction-summarizer calls are dropped via
///      `is_auxiliary` and never reach turn assembly — pre-profile they
///      collapsed into two `gen-*` synth-id sessions because their
///      first-user/first-assistant text was byte-identical boilerplate.)
///   2. Bug-fix-specific: every reconstructed `tool_use` block has a
///      non-empty parsed `input` object. Pre-fix, one or more `tool_use`
///      blocks per parallel-tool response ended up with `input: ""` because
///      the SSE accumulator ignored the `index` field; the lost JSON was
///      either dropped or attached to the wrong block.
#[tokio::test]
async fn openclaw_anthropic_parallel_tool_use_inputs_intact() {
    let Some((turns, calls)) = run_pcap_collecting_calls("openclaw-anthropic.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };

    let anthropic: Vec<_> = turns
        .iter()
        .filter(|t| t.wire_api == wa::ANTHROPIC)
        .collect();
    eprintln!(
        "openclaw-anthropic: {} anthropic turns, {} llm_calls",
        anthropic.len(),
        calls.len(),
    );
    for t in &anthropic {
        eprintln!(
            "  session={} status={:?} calls={} models={:?}",
            t.session_id, t.status, t.call_count, t.models_used,
        );
    }

    assert_eq!(
        anthropic.len(),
        4,
        "expected 4 turns; got {}",
        anthropic.len()
    );
    assert!(anthropic.iter().all(|t| t.agent_kind == "openclaw"));
    assert!(
        anthropic.iter().all(|t| t.status == TurnStatus::Complete),
        "all turns expected Complete"
    );
    let sessions: std::collections::BTreeSet<_> =
        anthropic.iter().map(|t| t.session_id.as_str()).collect();
    assert_eq!(
        sessions.len(),
        1,
        "expected 1 distinct session (compaction aux dropped); got {sessions:?}",
    );
    // Sole remaining session anchors on the first tool_use id of the
    // user-facing conversation. Compaction-summarizer calls were filtered
    // out by `OpenClawProfile::is_auxiliary` before turn assembly.
    assert!(
        sessions
            .iter()
            .all(|s| s.starts_with("toolu_") || s.starts_with("call_")),
        "main session_id must be a canonical tool id; got {sessions:?}",
    );

    // Bug-fix assertion: every tool_use block in every reconstructed response
    // body must carry a non-empty parsed input. An `input == ""` string is the
    // exact symptom of the pre-fix index-blind accumulator.
    let mut tool_use_blocks_seen = 0usize;
    let mut empty_input_block_ids: Vec<String> = Vec::new();
    for call in &calls {
        if call.wire_api != wa::ANTHROPIC {
            continue;
        }
        let Some(body) = call.response_body.as_deref() else {
            continue;
        };
        let v: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(blocks) = v.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            tool_use_blocks_seen += 1;
            let id = b
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("<no-id>")
                .to_string();
            match b.get("input") {
                Some(serde_json::Value::Object(o)) if !o.is_empty() => {} // healthy
                _ => empty_input_block_ids.push(id),
            }
        }
    }
    assert!(
        tool_use_blocks_seen >= 2,
        "fixture should contain at least one parallel-tool_use response; saw only {tool_use_blocks_seen} tool_use blocks",
    );
    assert!(
        empty_input_block_ids.is_empty(),
        "{} tool_use block(s) had empty/non-object input — accumulator regressed: {:?}",
        empty_input_block_ids.len(),
        empty_input_block_ids,
    );
}

/// Hermes Agent (Nous Research) capture over OpenAI `/v1/chat/completions`.
/// Hermes uses the upstream `openai-python` SDK with no Hermes-specific
/// headers (UA: `OpenAI/Python <ver>`), so identification relies on the
/// body fingerprint in `HermesProfile::matches` (≥2 of `skill_view`,
/// `skill_manage`, `skills_list`, `delegate_task`, `session_search`,
/// `cronjob` in `tools[]`).
///
/// The fixture contains one user-facing conversation followed by Hermes's
/// chat-title-generation one-shot. The two are independent on the wire and
/// emit as two separate AgentTurns by design (independent session_ids,
/// independent user-start, independent terminal). Asserts:
///   1. Exactly two OpenAI Chat turns.
///   2. The 4-call main conversation classifies as `hermes`. Its
///      `user_input_preview` is the user's prompt verbatim — the title-gen
///      prompt MUST NOT leak in here.
///   3. The 1-call title-generation call falls through to `generic` (no
///      Hermes tool markers in its body). Its `final_answer_preview`
///      matches the synthesized title.
#[tokio::test]
async fn hermes_openai_expects_hermes_main_and_generic_title_gen() {
    let Some(turns) = run_pcap("hermes-openai.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let chat: Vec<_> = turns
        .iter()
        .filter(|t| t.wire_api == wa::OPENAI_CHAT)
        .collect();
    eprintln!("hermes-openai: {} openai-chat turns", chat.len());
    for t in &chat {
        eprintln!(
            "  agent={} status={:?} calls={} user={:?} final={:?}",
            t.agent_kind,
            t.status,
            t.call_count,
            t.user_input_preview.as_deref().unwrap_or(""),
            t.final_answer_preview.as_deref().unwrap_or(""),
        );
    }
    assert_eq!(chat.len(), 2, "expected 2 turns; got {}", chat.len());
    assert!(
        chat.iter().all(|t| t.status == TurnStatus::Complete),
        "all turns expected Complete (both end with finish_reason=stop)",
    );

    let main = chat
        .iter()
        .find(|t| t.agent_kind == "hermes")
        .expect("expected one hermes-classified turn (main conversation)");
    assert_eq!(
        main.call_count, 4,
        "main conversation has 4 LLM calls (3 tool roundtrips + 1 final answer)",
    );
    let main_user = main.user_input_preview.as_deref().unwrap_or_default();
    assert!(
        main_user.starts_with("检查当前tokenscope"),
        "hermes turn user_input must be the user's prompt verbatim, got {main_user:?}",
    );

    let title = chat
        .iter()
        .find(|t| t.agent_kind == "generic")
        .expect("title-gen call must fall through to generic (no Hermes markers)");
    assert_eq!(
        title.call_count, 1,
        "title generation is a single one-shot call",
    );
    let title_final = title.final_answer_preview.as_deref().unwrap_or_default();
    assert!(
        title_final.contains("tokenscope"),
        "title-gen final_answer must reference the conversation topic, got {title_final:?}",
    );
    // The two turns must live in distinct session buckets — that's why
    // they're separate turns in the first place. If they collapsed into one
    // session, the tracker would partition them but this assertion guards
    // against any future session-id heuristic that accidentally fuses them.
    assert_ne!(
        main.session_id, title.session_id,
        "main conversation and title-gen call must occupy distinct session buckets",
    );
}

#[tokio::test]
async fn hermes_openai_pcap_shard_parity() {
    let Some(single) = run_pcap_sharded("hermes-openai.pcap", 1, 1).await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let multi = run_pcap_sharded("hermes-openai.pcap", 4, 4)
        .await
        .unwrap();

    let single_keys: std::collections::BTreeSet<_> = single
        .iter()
        .map(|t| {
            (
                t.session_id.clone(),
                t.agent_kind.clone(),
                t.call_count,
                t.status.to_string(),
            )
        })
        .collect();
    let multi_keys: std::collections::BTreeSet<_> = multi
        .iter()
        .map(|t| {
            (
                t.session_id.clone(),
                t.agent_kind.clone(),
                t.call_count,
                t.status.to_string(),
            )
        })
        .collect();
    assert_eq!(
        single_keys, multi_keys,
        "turn sets must match across shard counts",
    );
}

/// End-to-end unit test: two `LlmCall` records (no claude-cli UA) run through
/// `build_agent_call_info` + `TurnTracker` produce a single `AgentTurn` with
/// `agent_kind == "generic"` and `session_id == "toolu_pcap"`. The Anthropic
/// wire-api shape exercises the generic profile's `wa::ANTHROPIC` branch.
/// No pcap fixture — calls are constructed directly for speed and determinism.
#[tokio::test]
async fn generic_profile_anthropic_two_call_session() {
    use std::net::IpAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use ts_common::internal_metrics::{Metric, MetricsSystem};
    use ts_llm::agents::build_default_registry;
    use ts_llm::build_agent_call_info;
    use ts_llm::model::{AgentCall, ApiType, LlmCall};
    use ts_turn::tracker::{TrackerConfig, TurnTracker};
    use ts_turn::{TurnEvent, TurnStatus};

    fn make_call(req: &str, resp: &str, ts_us: i64, finish: Option<&str>) -> LlmCall {
        LlmCall {
            source_id: "test".into(),
            id: format!("c-{ts_us}"),
            wire_api: ts_llm::wire_apis::ANTHROPIC,
            model: "claude-3-5-sonnet".into(),
            api_type: ApiType::Chat,
            request_time: ts_us,
            response_time: Some(ts_us + 10_000),
            complete_time: Some(ts_us + 50_000),
            request_path: "/v1/messages".into(),
            is_stream: true,
            request_body: Some(req.into()),
            status_code: Some(200),
            finish_reason: finish.map(str::to_string),
            response_body: Some(resp.into()),
            input_tokens: Some(10),
            output_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: Some(50.0),
            e2e_latency_ms: Some(100.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 4444,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 443,
            response_id: None,
            // No claude-cli UA — falls to generic (anthropic wire-api branch).
            request_headers: vec![("User-Agent".into(), "anthropic/0.40 python/3.12".into())],
            response_headers: vec![],
        }
    }

    // Separate MetricsWorker for build_agent_call_info (wire-detection metrics).
    let mut llm_sys = MetricsSystem::new();
    let llm_metrics = llm_sys.register_worker(
        "test-llm",
        &[
            Metric::WireDetected,
            Metric::WireIgnored,
            Metric::LlmGenericToolIdCanonicalized,
            Metric::LlmGenericSessionIdSynthFailed,
        ],
    );
    let _llm_svc = llm_sys.start();

    // Separate MetricsWorker for TurnTracker (turn-assembly metrics).
    let mut turn_sys = MetricsSystem::new();
    let turn_metrics = turn_sys.register_worker(
        "test-turn",
        &[
            Metric::TurnCallsIngested,
            Metric::TurnCallsAuxiliary,
            Metric::TurnsCompleted,
            Metric::TurnCallsDroppedLate,
            Metric::TurnClosedByGrace,
            Metric::TurnClosedByIdle,
            Metric::TurnDiscardedNoUserStart,
        ],
    );
    let _turn_svc = turn_sys.start();

    let registry = Arc::new(build_default_registry());
    let wire_apis = Arc::new(ts_llm::wire_apis::build_default_wire_api_registry());

    // Call #1: user prompt → assistant tool_use (not terminal).
    let req1 = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"fix the bug"}]}]}"#;
    let resp1 = r#"{"content":[{"type":"tool_use","id":"toolu_pcap","name":"Read","input":{"path":"/x"}}],"stop_reason":"tool_use"}"#;
    let call1 = make_call(req1, resp1, 1_000_000, Some("tool_use"));
    let info1 = build_agent_call_info(&call1, &registry, &wire_apis, &llm_metrics).expect("info1");
    assert_eq!(info1.agent_kind, "generic");
    assert_eq!(info1.session_id, "toolu_pcap");
    assert_eq!(info1.is_user_turn_start, Some(true));
    assert!(!info1.is_turn_terminal, "tool_use is not terminal");

    // Call #2: tool_result → assistant text (terminal end_turn).
    let req2 = r#"{"messages":[
        {"role":"user","content":[{"type":"text","text":"fix the bug"}]},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_pcap","name":"Read","input":{"path":"/x"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_pcap","content":"file ok"}]}
    ]}"#;
    let resp2 =
        r#"{"content":[{"type":"text","text":"the bug is at line 42"}],"stop_reason":"end_turn"}"#;
    let call2 = make_call(req2, resp2, 2_000_000, Some("end_turn"));
    let info2 = build_agent_call_info(&call2, &registry, &wire_apis, &llm_metrics).expect("info2");
    assert_eq!(
        info2.session_id, "toolu_pcap",
        "call #2 must hit same session as call #1"
    );
    assert!(info2.is_turn_terminal, "end_turn is terminal");

    // Feed both into the tracker with a fixed wall-clock instant.
    // grace=1s (default). Use ingest_at with the same `now` so neither call
    // triggers grace on its own. Then force-flush via flush_all_at with a
    // wall-clock 2 s later (past the 1 s grace window).
    let mut tracker = TurnTracker::new(
        TrackerConfig {
            grace: Duration::from_secs(1),
            ..TrackerConfig::default()
        },
        turn_metrics,
    );
    let now = Instant::now();
    let mut events = Vec::new();
    events.extend(tracker.ingest_at(
        AgentCall {
            call: Arc::new(call1),
            agent: info1,
        },
        now,
    ));
    events.extend(tracker.ingest_at(
        AgentCall {
            call: Arc::new(call2),
            agent: info2,
        },
        now,
    ));
    // Force grace expiry by advancing wall clock past the grace window.
    let later = now + Duration::from_secs(2);
    events.extend(tracker.flush_all_at(later));

    let turns: Vec<_> = events
        .into_iter()
        .filter_map(|e| match e {
            TurnEvent::Completed(t) => Some(t),
        })
        .collect();
    assert_eq!(turns.len(), 1, "exactly one turn");
    let t = &turns[0];
    assert_eq!(t.session_id, "toolu_pcap");
    assert_eq!(t.agent_kind, "generic");
    assert_eq!(t.call_count, 2);
    assert_eq!(
        t.user_input_preview.as_deref(),
        Some("fix the bug"),
        "user_input_preview"
    );
    assert_eq!(
        t.final_answer_preview.as_deref(),
        Some("the bug is at line 42"),
        "final_answer_preview"
    );
    assert!(
        matches!(t.status, TurnStatus::Complete),
        "status must be Complete, got {:?}",
        t.status
    );
}

/// Gemini CLI (API-key mode) capture against a local proxy. Wire format is
/// the public Gemini AI Studio REST surface
/// (`POST /v1beta/models/{m}:streamGenerateContent`). Gemini CLI is a
/// multi-turn agent, but no dedicated `gemini-cli` profile exists yet — the
/// `generic` profile dispatches by `wire_api`.
///
/// Ground truth (from manual inspection of the 7 captured POSTs across 3 TCP
/// streams):
///
///   - Turn A: 4 calls — initial prompt + 3 tool roundtrips. Closes when
///     call 4's response is pure text (no functionCall) → Gemini wire `STOP`
///     does NOT trigger our synthetic `TOOL_USE` rewrite, so
///     `is_turn_terminal` evaluates true.
///   - Turn B: 3 calls — user follow-up prompt mid-conversation (re-arms
///     `is_user_turn_start`) + 2 more tool roundtrips. Closes the same way
///     as turn A.
///
/// Both turns share one synthesized `session_id` of form `tu-<16hex>`.
/// Gemini's wire protocol has no opaque tool-call ids, so
/// `first_assistant_sig_*` returns `ToolId(<fnv1a hash of canonical sig>)`
/// when the model turn carries any non-text part; both response-side and
/// request-history-side observations of the same model turn produce
/// identical hashes.
///
/// Catches two regressions:
///
///   1. Profile-match coverage: `GenericProfile::matches()` must include
///      `wa::GEMINI_AISTUDIO`. Without it, no profile matches and no turn
///      is emitted at all.
///   2. Sig variant correctness: `first_assistant_sig_from_*` must return
///      `ToolId(_)` (not `Text(_)`) when the model turn carries any
///      functionCall, otherwise `generic` profile's helper-shape one-shot
///      gate spuriously fires on call 1 (sig=Text + system_text non-empty
///      + no model history) and gives it a `gen-<helper_hash>` session_id
///      distinct from calls 2..7's `gen-<text_hash>`. The pcap then
///      shatters into 1 + N turns instead of 4 + 3.
#[tokio::test]
async fn gemini_cli_apikey_expects_two_turns_4_and_3() {
    let Some(turns) = run_pcap("gemini-cli-apikey.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let gemini: Vec<_> = turns
        .iter()
        .filter(|t| t.wire_api == wa::GEMINI_AISTUDIO)
        .collect();
    eprintln!("gemini-cli-apikey: {} gemini-aistudio turns", gemini.len());
    for t in &gemini {
        eprintln!(
            "  turn {} status={:?} calls={} session={}",
            t.turn_id, t.status, t.call_count, t.session_id,
        );
    }

    assert_eq!(gemini.len(), 2, "expected 2 turns; got {}", gemini.len());

    // No dedicated profile yet — generic dispatcher handles Gemini.
    assert!(
        gemini.iter().all(|t| t.agent_kind == "generic"),
        "agent_kind should be 'generic'; got {:?}",
        gemini.iter().map(|t| &t.agent_kind).collect::<Vec<_>>(),
    );

    // All 7 calls must share one session_id — proves the sig algorithm is
    // stable across the resp/req-history boundary AND the helper-shape
    // gate is correctly skipped for tools-bearing first calls.
    let sessions: std::collections::BTreeSet<_> =
        gemini.iter().map(|t| t.session_id.as_str()).collect();
    assert_eq!(
        sessions.len(),
        1,
        "all turns must share one session_id; got {sessions:?}",
    );

    // Turn split: 4 + 3 (order-insensitive — turn assembly emits in
    // completion order, which depends on tracker flush behavior).
    let mut counts: Vec<u32> = gemini.iter().map(|t| t.call_count).collect();
    counts.sort();
    assert_eq!(
        counts,
        vec![3, 4],
        "expected calls split 4+3; got {counts:?}",
    );
    assert_eq!(
        gemini.iter().map(|t| t.call_count).sum::<u32>(),
        7,
        "total LlmCall count across turns must equal 7",
    );
}
