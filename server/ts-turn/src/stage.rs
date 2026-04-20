//! Turn tracking stage: spawns T parallel TurnTracker tasks, each shard
//! keyed by hash(session_id) % T. Each shard owns its own tracker and
//! emits finalized LlmTurns to turns_tx. LlmCalls flow directly from
//! llm-proc to storage (Arc<LlmCall> shared read-only); this stage does
//! not forward them.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use ts_common::internal_metrics::{Metric, MetricsSystem};
use ts_llm::model::TurnShardInput;
use ts_llm::profile::ProfileRegistry;

use crate::model::LlmTurn;
use crate::tracker::{TrackerConfig, TurnEvent, TurnTracker};

/// Spawn one turn-tracker task per shard (inferred from `shard_rxs.len()`).
/// Panics on empty `shard_rxs` — a wiring bug in the composition root.
pub fn spawn_turn_stage(
    tracker_cfg: TrackerConfig,
    shard_rxs: Vec<mpsc::Receiver<TurnShardInput>>,
    turns_tx: mpsc::Sender<LlmTurn>,
    registry: Arc<ProfileRegistry>,
    metrics_sys: &mut MetricsSystem,
) -> Vec<JoinHandle<()>> {
    assert!(
        !shard_rxs.is_empty(),
        "spawn_turn_stage: shard_rxs must be non-empty"
    );
    let mut handles = Vec::with_capacity(shard_rxs.len());
    for (i, mut rx) in shard_rxs.into_iter().enumerate() {
        let turns_tx = turns_tx.clone();
        let registry = registry.clone();
        let worker_metrics = metrics_sys.register_worker(
            &format!("turn.{i}"),
            &[
                Metric::TurnCallsIngested,
                Metric::TurnCallsAuxiliary,
                Metric::TurnsCompleted,
                Metric::TurnsTimedOut,
                Metric::TurnReorderOrphan,
                Metric::TurnFinalizedByGrace,
                Metric::TurnFinalizedByIdle,
                Metric::TurnDiscardedNoUserStart,
            ],
        );
        handles.push(tokio::spawn(async move {
            let shard = i;
            let mut tracker = TurnTracker::new(registry, tracker_cfg, worker_metrics);
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
                    TurnShardInput::Heartbeat { ts, stream_id } => {
                        for ev in tracker.advance_time(ts, &stream_id) {
                            let TurnEvent::Completed(t) = ev;
                            if turns_tx.send(t).await.is_err() {
                                break 'main "downstream_closed";
                            }
                        }
                    }
                }
            };
            match reason {
                "upstream_eof" => {
                    // Only drain remaining turns when the upstream closed cleanly.
                    // If downstream is already gone, flush has nowhere to go.
                    for ev in tracker.flush_all() {
                        let TurnEvent::Completed(t) = ev;
                        let _ = turns_tx.send(t).await;
                    }
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
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_llm::model::{ApiType, CallIdentity, FinishReason, IdentifiedCall, LlmCall};
    use ts_llm::profiles::build_default_registry;
    use ts_llm::provider_names as pn;

    /// `is_user_start`: true ⇒ text body (new-turn marker); false ⇒ tool_result body (continuation).
    fn anthropic_call(
        session: &str,
        ts_us: i64,
        finish: FinishReason,
        is_user_start: bool,
    ) -> LlmCall {
        let body = if is_user_start {
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"go"}]}]}"#
        } else {
            r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#
        };
        LlmCall {
            stream_id: String::new(),
            id: format!("c-{ts_us}"),
            provider: pn::ANTHROPIC,
            model: "claude".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: ts_us,
            response_time: Some(ts_us + 100_000),
            complete_time: Some(ts_us + 200_000),
            request_path: "/v1/messages".into(),
            is_stream: true,
            request_body: Some(body.to_string()),
            status_code: Some(200),
            finish_reason: Some(finish),
            response_body: None,
            input_tokens: Some(1),
            output_tokens: Some(1),
            total_tokens: Some(2),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: None,
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
        }
    }

    fn id_for(session: &str) -> CallIdentity {
        CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".into(),
            session_id: session.into(),
            turn_id_hint: None,
        }
    }

    #[tokio::test]
    async fn single_shard_produces_turn() {
        let (shard_tx, shard_rx) = mpsc::channel::<TurnShardInput>(16);
        let (turns_tx, mut turns_rx) = mpsc::channel::<LlmTurn>(16);

        let mut metrics_sys = MetricsSystem::new();
        spawn_turn_stage(
            TrackerConfig::default(),
            vec![shard_rx],
            turns_tx.clone(),
            Arc::new(build_default_registry()),
            &mut metrics_sys,
        );
        let _svc = metrics_sys.start();
        drop(turns_tx);

        let c1 = Arc::new(anthropic_call("S", 1_000_000, FinishReason::ToolUse, true));
        let c2 = Arc::new(anthropic_call(
            "S",
            2_000_000,
            FinishReason::Complete,
            false,
        ));
        let (id1, id2) = (c1.id.clone(), c2.id.clone());

        shard_tx
            .send(TurnShardInput::Call(IdentifiedCall {
                call: c1,
                identity: id_for("S"),
            }))
            .await
            .unwrap();
        shard_tx
            .send(TurnShardInput::Call(IdentifiedCall {
                call: c2,
                identity: id_for("S"),
            }))
            .await
            .unwrap();
        drop(shard_tx);

        let mut turns = Vec::new();
        while let Some(t) = turns_rx.recv().await {
            turns.push(t);
        }
        assert_eq!(turns.len(), 1, "one complete turn expected");
        assert_eq!(turns[0].call_ids, vec![id1, id2]);
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
        let (turns_tx, mut turns_rx) = mpsc::channel::<LlmTurn>(64);

        let mut metrics_sys = MetricsSystem::new();
        spawn_turn_stage(
            TrackerConfig::default(),
            shard_rxs,
            turns_tx.clone(),
            Arc::new(build_default_registry()),
            &mut metrics_sys,
        );
        let _svc = metrics_sys.start();
        drop(turns_tx);

        for (i, tx) in shard_txs.iter().enumerate() {
            let session = format!("S{i}");
            let c1 = Arc::new(anthropic_call(
                &session,
                1_000_000 + i as i64,
                FinishReason::ToolUse,
                true,
            ));
            let c2 = Arc::new(anthropic_call(
                &session,
                2_000_000 + i as i64,
                FinishReason::Complete,
                false,
            ));
            tx.send(TurnShardInput::Call(IdentifiedCall {
                call: c1,
                identity: id_for(&session),
            }))
            .await
            .unwrap();
            tx.send(TurnShardInput::Call(IdentifiedCall {
                call: c2,
                identity: id_for(&session),
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
        assert!(turns.iter().all(|t| t.call_ids.len() == 2));
    }

    #[tokio::test]
    #[should_panic(expected = "spawn_turn_stage: shard_rxs must be non-empty")]
    async fn panics_on_empty_shard_rxs() {
        let (_turns_tx, _turns_rx) = mpsc::channel::<LlmTurn>(1);
        let mut metrics_sys = MetricsSystem::new();
        spawn_turn_stage(
            TrackerConfig::default(),
            vec![],
            _turns_tx,
            Arc::new(build_default_registry()),
            &mut metrics_sys,
        );
    }
}
