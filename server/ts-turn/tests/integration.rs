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
    assert!(chat.iter().all(|t| t.agent_kind == "generic-openai-chat"));
    assert!(
        chat.iter().all(|t| t.status == TurnStatus::Complete),
        "all turns expected Complete"
    );
    let sessions: std::collections::BTreeSet<_> =
        chat.iter().map(|t| t.session_id.as_str()).collect();
    assert_eq!(sessions.len(), 2, "expected 2 distinct sessions; got {sessions:?}");
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

/// End-to-end unit test: two `LlmCall` records (no claude-cli UA) run through
/// `build_agent_call_info` + `TurnTracker` produce a single `AgentTurn` with
/// `agent_kind == "generic-anthropic"` and `session_id == "toolu_pcap"`.
/// No pcap fixture — calls are constructed directly for speed and determinism.
#[tokio::test]
async fn generic_anthropic_two_call_session() {
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
            // No claude-cli UA — falls to generic-anthropic.
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
    let info1 =
        build_agent_call_info(&call1, &registry, &wire_apis, &llm_metrics).expect("info1");
    assert_eq!(info1.agent_kind, "generic-anthropic");
    assert_eq!(info1.session_id, "toolu_pcap");
    assert_eq!(info1.is_user_turn_start, Some(true));
    assert!(!info1.is_turn_terminal, "tool_use is not terminal");

    // Call #2: tool_result → assistant text (terminal end_turn).
    let req2 = r#"{"messages":[
        {"role":"user","content":[{"type":"text","text":"fix the bug"}]},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_pcap","name":"Read","input":{"path":"/x"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_pcap","content":"file ok"}]}
    ]}"#;
    let resp2 = r#"{"content":[{"type":"text","text":"the bug is at line 42"}],"stop_reason":"end_turn"}"#;
    let call2 = make_call(req2, resp2, 2_000_000, Some("end_turn"));
    let info2 =
        build_agent_call_info(&call2, &registry, &wire_apis, &llm_metrics).expect("info2");
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
    assert_eq!(t.agent_kind, "generic-anthropic");
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
