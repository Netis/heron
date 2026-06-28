//! Turn tracking stage: spawns T parallel TurnTracker tasks, each shard
//! keyed by hash(session_id) % T. Each shard owns its own tracker and
//! emits finalized AgentTurns to turns_tx. LlmCalls flow directly from
//! llm-proc to storage (Arc<LlmCall> shared read-only); this stage does
//! not forward them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use h_common::internal_metrics::{Metric, MetricsSystem};
use h_llm::model::TurnShardInput;

use crate::model::{ActiveTraceRegistry, Trace};
use crate::tracker::{TrackerConfig, TurnEvent, TurnTracker};

/// Spawn one turn-tracker task per shard (inferred from `shard_rxs.len()`).
/// Panics on empty `shard_rxs` — a wiring bug in the composition root.
///
/// `active_registry` is the shared in-memory map of in-progress turns,
/// written by every tracker on each ingest and read by the API. `None`
/// disables the in-progress visibility path (used by tests that don't
/// need it).
pub fn spawn_turn_stage(
    tracker_cfg: TrackerConfig,
    shard_rxs: Vec<mpsc::Receiver<TurnShardInput>>,
    turns_tx: mpsc::Sender<Trace>,
    metrics_sys: &mut MetricsSystem,
    active_registry: Option<ActiveTraceRegistry>,
) -> Vec<JoinHandle<()>> {
    assert!(
        !shard_rxs.is_empty(),
        "spawn_turn_stage: shard_rxs must be non-empty"
    );

    // Per-shard gauges updated after every tracker mutation. The probe sums
    // across shards to report `turn_calls_buffered` — total in-flight LLM
    // calls sitting in per-session turn buffers waiting to be grouped into
    // a finalized turn. NOTE: this counts CALLS, not open turns; a single
    // conversation with N concurrent calls contributes N. For the "open
    // agent turns" signal the dashboard uses, see `agent_turns_open`
    // (registered on the global svc against ActiveTraceRegistry).
    let active_gauges: Vec<Arc<AtomicU64>> = (0..shard_rxs.len())
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    let gauges_for_probe = active_gauges.clone();
    metrics_sys.register_queue_probe(Metric::TurnActive, move || {
        gauges_for_probe
            .iter()
            .map(|g| g.load(Ordering::Relaxed))
            .sum()
    });

    let mut handles = Vec::with_capacity(shard_rxs.len());
    for (i, (mut rx, active_gauge)) in shard_rxs
        .into_iter()
        .zip(active_gauges.into_iter())
        .enumerate()
    {
        let turns_tx = turns_tx.clone();
        let worker_metrics = metrics_sys.register_worker(
            &format!("turn.{i}"),
            &[
                Metric::TurnCallsIngested,
                Metric::TurnCallsAuxiliary,
                Metric::TurnsCompleted,
                Metric::TurnCallsDroppedLate,
                Metric::TurnClosedByGrace,
                Metric::TurnClosedByIdle,
                Metric::TurnDiscardedNoUserStart,
                Metric::TurnKeptByPidAttribution,
                Metric::TurnHeartbeatsReceived,
            ],
        );
        let registry_clone = active_registry.clone();
        handles.push(tokio::spawn(async move {
            let shard = i;
            let mut tracker =
                TurnTracker::with_registry(tracker_cfg, worker_metrics, registry_clone);
            let reason = 'main: loop {
                let input = match rx.recv().await {
                    Some(x) => x,
                    None => break 'main "upstream_eof",
                };
                match input {
                    TurnShardInput::Call(identified) => {
                        for ev in tracker.ingest(identified) {
                            let TurnEvent::Completed(t) = ev;
                            if turns_tx.send(t).await.is_err() {
                                break 'main "downstream_closed";
                            }
                        }
                        for ev in tracker.sweep() {
                            let TurnEvent::Completed(t) = ev;
                            if turns_tx.send(t).await.is_err() {
                                break 'main "downstream_closed";
                            }
                        }
                    }
                    TurnShardInput::Heartbeat { ts, source_id } => {
                        for ev in tracker.advance_time(ts, &source_id) {
                            let TurnEvent::Completed(t) = ev;
                            if turns_tx.send(t).await.is_err() {
                                break 'main "downstream_closed";
                            }
                        }
                    }
                }
                active_gauge.store(tracker.active_count() as u64, Ordering::Relaxed);
            };
            match reason {
                "upstream_eof" => {
                    // Only drain remaining turns when the upstream closed cleanly.
                    // If downstream is already gone, flush has nowhere to go.
                    for ev in tracker.flush_all() {
                        let TurnEvent::Completed(t) = ev;
                        let _ = turns_tx.send(t).await;
                    }
                    active_gauge.store(tracker.active_count() as u64, Ordering::Relaxed);
                    tracing::debug!(shard, "turn worker stopping: upstream EOF");
                }
                r => {
                    tracing::warn!(shard, reason = r, "turn worker stopping: downstream closed");
                }
            }
        }));
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;
    use h_llm::agents::build_default_registry;
    use h_llm::model::{AgentCall, AgentCallInfo, ApiType, LlmCall};
    use h_llm::wire_apis as wa;
    use h_llm::wire_apis::build_default_wire_api_registry;
    use std::net::IpAddr;
    use std::sync::Arc;

    fn llm_test_metrics() -> h_common::internal_metrics::MetricsWorker {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test-llm",
            &[
                Metric::WireDetected,
                Metric::WireIgnored,
                Metric::LlmGenericToolIdCanonicalized,
                Metric::LlmGenericSessionIdSynthFailed,
            ],
        );
        let _svc = sys.start();
        w
    }

    /// Build a full `AgentCallInfo` for `call` via the production pipeline —
    /// keeps test fixtures aligned with what h-llm actually produces.
    fn call_info_for(call: &LlmCall) -> AgentCallInfo {
        let reg = build_default_registry();
        let wa_reg = build_default_wire_api_registry();
        let metrics = llm_test_metrics();
        h_llm::build_agent_call_info(
            call,
            &reg,
            &wa_reg,
            &h_llm::agent_classifier::ClassifierConfig::default(),
            &metrics,
        )
        .expect("call info")
    }

    /// `is_user_start`: true ⇒ text body (new-turn marker); false ⇒ tool_result body (continuation).
    fn anthropic_call(session: &str, ts_us: i64, finish: &str, is_user_start: bool) -> LlmCall {
        let body = if is_user_start {
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"go"}]}]}"#
        } else {
            r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#
        };
        LlmCall {
            source_id: String::new(),
            id: format!("c-{ts_us}"),
            wire_api: wa::ANTHROPIC,
            model: "claude".into(),
            api_type: ApiType::Chat,
            request_time: ts_us,
            response_time: Some(ts_us + 100_000),
            complete_time: Some(ts_us + 200_000),
            request_path: "/v1/messages".into(),
            is_stream: true,
            request_body: Some(body.to_string()),
            status_code: Some(200),
            finish_reason: Some(finish.to_string()),
            response_body: None,
            input_tokens: Some(1),
            output_tokens: Some(1),
            total_tokens: Some(2),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![
                ("User-Agent".into(), "claude-cli/2.1".into()),
                ("X-Claude-Code-Session-Id".into(), session.into()),
            ],
            response_headers: vec![],
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
            body_bytes_dropped: 0,
            attribution: h_common::attribution::AttributionInfo::ambiguous(),
            process: None,
        }
    }

    #[tokio::test]
    async fn single_shard_produces_turn() {
        let (shard_tx, shard_rx) = mpsc::channel::<TurnShardInput>(16);
        let (turns_tx, mut turns_rx) = mpsc::channel::<Trace>(16);

        let mut metrics_sys = MetricsSystem::new();
        spawn_turn_stage(
            TrackerConfig::default(),
            vec![shard_rx],
            turns_tx.clone(),
            &mut metrics_sys,
            None,
        );
        let _svc = metrics_sys.start();
        drop(turns_tx);

        let c1 = anthropic_call("S", 1_000_000, "tool_use", true);
        let c2 = anthropic_call("S", 2_000_000, "end_turn", false);
        let (id1, id2) = (c1.id.clone(), c2.id.clone());
        let agent1 = call_info_for(&c1);
        let agent2 = call_info_for(&c2);

        shard_tx
            .send(TurnShardInput::Call(AgentCall {
                call: Arc::new(c1),
                agent: agent1,
            }))
            .await
            .unwrap();
        shard_tx
            .send(TurnShardInput::Call(AgentCall {
                call: Arc::new(c2),
                agent: agent2,
            }))
            .await
            .unwrap();
        drop(shard_tx);

        let mut turns = Vec::new();
        while let Some(t) = turns_rx.recv().await {
            turns.push(t);
        }
        assert_eq!(turns.len(), 1, "one complete turn expected");
        assert_eq!(turns[0].span_ids, vec![id1, id2]);
    }

    #[tokio::test]
    async fn four_shards_isolate_by_session() {
        let mut shard_txs = Vec::with_capacity(4);
        let mut shard_rxs = Vec::with_capacity(4);
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel::<TurnShardInput>(16);
            shard_txs.push(tx);
            shard_rxs.push(rx);
        }
        let (turns_tx, mut turns_rx) = mpsc::channel::<Trace>(64);

        let mut metrics_sys = MetricsSystem::new();
        spawn_turn_stage(
            TrackerConfig::default(),
            shard_rxs,
            turns_tx.clone(),
            &mut metrics_sys,
            None,
        );
        let _svc = metrics_sys.start();
        drop(turns_tx);

        for (i, tx) in shard_txs.iter().enumerate() {
            let session = format!("S{i}");
            let c1 = anthropic_call(&session, 1_000_000 + i as i64, "tool_use", true);
            let c2 = anthropic_call(&session, 2_000_000 + i as i64, "end_turn", false);
            let agent1 = call_info_for(&c1);
            let agent2 = call_info_for(&c2);
            tx.send(TurnShardInput::Call(AgentCall {
                call: Arc::new(c1),
                agent: agent1,
            }))
            .await
            .unwrap();
            tx.send(TurnShardInput::Call(AgentCall {
                call: Arc::new(c2),
                agent: agent2,
            }))
            .await
            .unwrap();
        }
        drop(shard_txs);

        let mut turns = Vec::new();
        while let Some(t) = turns_rx.recv().await {
            turns.push(t);
        }
        assert_eq!(turns.len(), 4, "one turn per shard");
        let sessions: std::collections::HashSet<_> =
            turns.iter().map(|t| t.session_id.clone()).collect();
        assert_eq!(sessions.len(), 4);
        assert!(turns.iter().all(|t| t.span_ids.len() == 2));
    }

    #[tokio::test]
    #[should_panic(expected = "spawn_turn_stage: shard_rxs must be non-empty")]
    async fn panics_on_empty_shard_rxs() {
        let (_turns_tx, _turns_rx) = mpsc::channel::<Trace>(1);
        let mut metrics_sys = MetricsSystem::new();
        spawn_turn_stage(
            TrackerConfig::default(),
            vec![],
            _turns_tx,
            &mut metrics_sys,
            None,
        );
    }
}
