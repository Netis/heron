//! LLM extraction stage: spawns N parallel LlmProcessor tasks, one per input
//! receiver. Each task owns its own LlmProcessor (sharing the
//! AgentProfileRegistry via Arc) and fans out each produced event to up to
//! three independent downstream destinations:
//!
//! * Every `LlmEvent` (Start and Complete) → one metrics shard, chosen by
//!   `hash(wire_api, model, server_ip) % M`.
//! * Every `LlmEvent::Complete` → `calls_tx` as `Arc<LlmCall>` (every call
//!   reaches storage regardless of agent attribution).
//! * `LlmEvent::Complete` with `agent.is_some()` → one turn shard, chosen
//!   by `hash(session_id) % T`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use ts_common::internal_metrics::{Metric, MetricsSystem};
use ts_protocol::joiner::HttpJoinerEvent;

use crate::model::{AgentCall, LlmCall, LlmEvent, TurnShardInput};
use crate::processor::LlmProcessor;
use crate::profile::AgentProfileRegistry;
use crate::wire_api_registry::WireApiRegistry;

/// Spawn N parallel LLM-extraction tasks, one per input receiver. Each task
/// owns its own `LlmProcessor` (sharing the `WireApiRegistry` and
/// `AgentProfileRegistry` via `Arc`) and fans out each produced event to up to
/// three downstream destinations:
///
/// * Every `LlmEvent` (Start and Complete) → one metrics shard, chosen by
///   `hash(wire_api, model, server_ip) % metrics_shard_txs.len()`.
/// * Every `LlmEvent::Complete` → `calls_tx` as `Arc<LlmCall>` (every call
///   reaches storage regardless of agent attribution).
/// * `LlmEvent::Complete` with `agent.is_some()` → one turn shard, chosen
///   by `hash(session_id) % turn_shard_txs.len()`.
pub fn spawn_llm_stage(
    event_rxs: Vec<mpsc::Receiver<HttpJoinerEvent>>,
    turn_shard_txs: Vec<mpsc::Sender<TurnShardInput>>,
    metrics_shard_txs: Vec<mpsc::Sender<LlmEvent>>,
    calls_tx: mpsc::Sender<Arc<LlmCall>>,
    wire_apis: Arc<WireApiRegistry>,
    registry: Arc<AgentProfileRegistry>,
    metrics_sys: &mut MetricsSystem,
) -> Vec<JoinHandle<()>> {
    assert!(
        !metrics_shard_txs.is_empty(),
        "spawn_llm_stage: metrics_shard_txs must be non-empty"
    );
    assert!(
        !turn_shard_txs.is_empty(),
        "spawn_llm_stage: turn_shard_txs must be non-empty"
    );
    let turn_shard_txs = Arc::new(turn_shard_txs);
    let metrics_shard_txs = Arc::new(metrics_shard_txs);

    let mut handles = Vec::with_capacity(event_rxs.len());
    for (i, mut rx) in event_rxs.into_iter().enumerate() {
        let wire_apis = wire_apis.clone();
        let reg = registry.clone();
        let turn_txs = turn_shard_txs.clone();
        let metrics_txs = metrics_shard_txs.clone();
        let calls_tx = calls_tx.clone();
        let worker_metrics = metrics_sys.register_worker(
            &format!("llm.{i}"),
            &[
                Metric::WireDetected,
                Metric::WireIgnored,
                Metric::LlmCallsWithAgent,
                Metric::LlmCallsWithoutAgent,
            ],
        );
        handles.push(tokio::spawn(async move {
            let shard = i;
            let mut processor = LlmProcessor::new(wire_apis, reg, worker_metrics.clone());
            let reason = 'main: loop {
                let event = match rx.recv().await {
                    Some(e) => e,
                    None => break 'main "upstream_eof",
                };
                for llm_event in processor.process(event) {
                    match llm_event {
                        LlmEvent::Heartbeat { ts, ref source_id } => {
                            for tx in metrics_txs.iter() {
                                let _ = tx.try_send(LlmEvent::Heartbeat {
                                    ts,
                                    source_id: source_id.clone(),
                                });
                            }
                            for tx in turn_txs.iter() {
                                let _ = tx.try_send(TurnShardInput::Heartbeat {
                                    ts,
                                    source_id: source_id.clone(),
                                });
                            }
                        }
                        other => {
                            let metrics_idx = metrics_shard_index(&other, metrics_txs.len());
                            if metrics_txs[metrics_idx].send(other.clone()).await.is_err() {
                                break 'main "downstream_closed_metrics";
                            }
                            if let LlmEvent::Complete { call, agent } = other {
                                if agent.is_some() {
                                    worker_metrics.counter(Metric::LlmCallsWithAgent).inc();
                                } else {
                                    worker_metrics.counter(Metric::LlmCallsWithoutAgent).inc();
                                }
                                if calls_tx.send(call.clone()).await.is_err() {
                                    break 'main "downstream_closed_calls";
                                }
                                if let Some(id) = agent {
                                    let idx = turn_shard_index(
                                        &call.source_id,
                                        &id.session_id,
                                        turn_txs.len(),
                                    );
                                    let ic = AgentCall { call, agent: id };
                                    if turn_txs[idx].send(TurnShardInput::Call(ic)).await.is_err() {
                                        break 'main "downstream_closed_turn";
                                    }
                                }
                            }
                        }
                    }
                }
            };
            match reason {
                "upstream_eof" => {
                    tracing::debug!(shard, "llm worker stopping: upstream EOF");
                }
                r => {
                    tracing::warn!(shard, reason = r, "llm worker stopping: downstream closed");
                }
            }
        }));
    }
    handles
}

fn turn_shard_index(source_id: &str, session_id: &str, n: usize) -> usize {
    let mut h = DefaultHasher::new();
    source_id.hash(&mut h);
    session_id.hash(&mut h);
    (h.finish() as usize) % n
}

fn metrics_shard_index(event: &LlmEvent, n: usize) -> usize {
    let (wire_api, model, server_ip) = match event {
        LlmEvent::Start(s) => (s.wire_api, s.model.as_str(), s.server_ip),
        LlmEvent::Complete { call, .. } => (call.wire_api, call.model.as_str(), call.server_ip),
        LlmEvent::Heartbeat { .. } => {
            unreachable!("metrics_shard_index called with Heartbeat event")
        }
    };
    let mut h = DefaultHasher::new();
    wire_api.hash(&mut h);
    model.hash(&mut h);
    server_ip.hash(&mut h);
    (h.finish() as usize) % n
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_protocol::joiner::HttpJoiner;
    use ts_protocol::model::{HttpParseEvent, HttpRequestData, HttpResponseData};
    use ts_protocol::net::FlowKey;

    use crate::agents::build_default_registry;
    use crate::model::{LlmCall, TurnShardInput};
    use crate::wire_apis as wa;
    use crate::wire_apis::build_default_wire_api_registry;
    use ts_common::internal_metrics::MetricsSystem;

    /// Feed a request + response pair through an ad-hoc HttpJoiner and return
    /// the resulting `HttpJoinerEvent`s. Mirrors what the real pipeline stage
    /// does; keeps the tests exercising the paired path end-to-end.
    fn joiner_events_for(req: HttpRequestData, resp: HttpResponseData) -> Vec<HttpJoinerEvent> {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test-joiner",
            &[
                Metric::HttpJoinerDone,
                Metric::HttpJoinerUnpaired,
                Metric::HttpJoinerExpired,
            ],
        );
        let _svc = sys.start();
        let mut joiner = HttpJoiner::new(w);
        let mut out = joiner.process(HttpParseEvent::HttpRequest(req));
        out.extend(joiner.process(HttpParseEvent::HttpResponse(resp)));
        out
    }

    fn flow_key(port: u16) -> FlowKey {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        FlowKey::new(String::new(), ip, port, ip, 8080)
    }

    fn openai_request(fk: FlowKey, ts_us: i64) -> HttpRequestData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        });
        HttpRequestData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            version: 1,
            headers: vec![
                ("authorization".to_string(), "Bearer sk-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: ts_us,
        }
    }

    fn openai_response(fk: FlowKey, ts_us: i64) -> HttpResponseData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        // content: null and no tool_calls so GenericOpenAiChatProfile.extract_ids
        // returns None — the call reaches calls_tx but not the turn shard.
        let body = serde_json::json!({
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": null}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        });
        HttpResponseData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(body.to_string()),
            first_byte_timestamp_us: ts_us + 100_000,
            complete_timestamp_us: ts_us + 200_000,
        }
    }

    fn claude_cli_request(fk: FlowKey, ts_us: i64, session: &str) -> HttpRequestData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "claude-sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        HttpRequestData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            version: 1,
            headers: vec![
                ("user-agent".to_string(), "claude-cli/2.1.98".to_string()),
                ("x-claude-code-session-id".to_string(), session.to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: ts_us,
        }
    }

    fn anthropic_response(fk: FlowKey, ts_us: i64) -> HttpResponseData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "id": "msg_01",
            "model": "claude-sonnet",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        HttpResponseData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(body.to_string()),
            first_byte_timestamp_us: ts_us + 100_000,
            complete_timestamp_us: ts_us + 200_000,
        }
    }

    #[tokio::test]
    async fn identified_call_fans_out_to_turn_shard_and_calls_tx_and_metrics() {
        let (event_tx, event_rx) = mpsc::channel::<HttpJoinerEvent>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnShardInput>(16);
        let (metrics_tx, mut metrics_rx) = mpsc::channel::<crate::model::LlmEvent>(16);
        let (calls_tx, mut calls_rx) = mpsc::channel::<Arc<LlmCall>>(16);

        let mut metrics_sys = MetricsSystem::new();
        spawn_llm_stage(
            vec![event_rx],
            vec![turn_tx],
            vec![metrics_tx],
            calls_tx,
            Arc::new(build_default_wire_api_registry()),
            Arc::new(build_default_registry()),
            &mut metrics_sys,
        );
        let _svc = metrics_sys.start();

        let fk = flow_key(5000);
        for ev in joiner_events_for(
            claude_cli_request(fk.clone(), 1_000_000, "S1"),
            anthropic_response(fk, 1_000_000),
        ) {
            event_tx.send(ev).await.unwrap();
        }
        drop(event_tx);

        let turn_input = turn_rx.recv().await.expect("turn shard should receive");
        let turn = match turn_input {
            TurnShardInput::Call(ic) => ic,
            TurnShardInput::Heartbeat { .. } => panic!("expected Call, got Heartbeat"),
        };
        assert_eq!(turn.agent.session_id, "S1");

        let call = calls_rx
            .recv()
            .await
            .expect("calls_tx should receive identified call");
        assert_eq!(call.wire_api, wa::ANTHROPIC);

        let mut start = false;
        let mut complete = false;
        while let Some(ev) = metrics_rx.recv().await {
            match ev {
                crate::model::LlmEvent::Start(_) => start = true,
                crate::model::LlmEvent::Complete { .. } => complete = true,
                crate::model::LlmEvent::Heartbeat { .. } => {}
            }
        }
        assert!(start && complete);
    }

    #[tokio::test]
    async fn unidentified_call_skips_turn_shard_still_reaches_calls_tx_and_metrics() {
        let (event_tx, event_rx) = mpsc::channel::<HttpJoinerEvent>(16);
        // Create an extra turn_tx to keep the channel open during the timeout assertion.
        // Without this, the spawned task drops its turn_tx clone on exit (because event_tx
        // is dropped), closing the channel before the 50 ms window, causing recv() to
        // return None immediately and the is_err() assertion to fail.
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnShardInput>(16);
        let _turn_tx_sentinel = turn_tx.clone(); // keeps channel open through the assertion
        let (metrics_tx, mut metrics_rx) = mpsc::channel::<crate::model::LlmEvent>(16);
        let (calls_tx, mut calls_rx) = mpsc::channel::<Arc<LlmCall>>(16);

        let mut metrics_sys = MetricsSystem::new();
        spawn_llm_stage(
            vec![event_rx],
            vec![turn_tx],
            vec![metrics_tx],
            calls_tx,
            Arc::new(build_default_wire_api_registry()),
            Arc::new(build_default_registry()),
            &mut metrics_sys,
        );
        let _svc = metrics_sys.start();

        let fk = flow_key(5000);
        for ev in joiner_events_for(
            openai_request(fk.clone(), 1_000_000),
            openai_response(fk, 1_000_000),
        ) {
            event_tx.send(ev).await.unwrap();
        }
        drop(event_tx);

        let call = calls_rx.recv().await.expect("calls_tx should receive");
        assert_eq!(call.wire_api, wa::OPENAI_CHAT);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), turn_rx.recv())
                .await
                .is_err(),
            "turn shard must stay empty for unidentified calls"
        );
        drop(_turn_tx_sentinel);

        let mut count = 0;
        while metrics_rx.recv().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn turn_shard_index_stable_by_session_id_hash() {
        let (event_tx, event_rx) = mpsc::channel::<HttpJoinerEvent>(16);
        let mut turn_txs = Vec::with_capacity(4);
        let mut turn_rxs = Vec::with_capacity(4);
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel::<TurnShardInput>(16);
            turn_txs.push(tx);
            turn_rxs.push(rx);
        }
        let (metrics_tx, _metrics_rx) = mpsc::channel::<crate::model::LlmEvent>(64);
        let (calls_tx, mut _calls_rx) = mpsc::channel::<Arc<LlmCall>>(64);
        let drain = tokio::spawn(async move { while _calls_rx.recv().await.is_some() {} });

        let mut metrics_sys = MetricsSystem::new();
        spawn_llm_stage(
            vec![event_rx],
            turn_txs,
            vec![metrics_tx],
            calls_tx,
            Arc::new(build_default_wire_api_registry()),
            Arc::new(build_default_registry()),
            &mut metrics_sys,
        );
        let _svc = metrics_sys.start();

        let fk1 = flow_key(5000);
        for ev in joiner_events_for(
            claude_cli_request(fk1.clone(), 1_000_000, "SAME"),
            anthropic_response(fk1, 1_000_000),
        ) {
            event_tx.send(ev).await.unwrap();
        }
        let fk2 = flow_key(5001);
        for ev in joiner_events_for(
            claude_cli_request(fk2.clone(), 2_000_000, "SAME"),
            anthropic_response(fk2, 2_000_000),
        ) {
            event_tx.send(ev).await.unwrap();
        }
        drop(event_tx);

        let mut non_empty = 0;
        for mut rx in turn_rxs {
            let got_any = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .ok()
                .flatten();
            if got_any.is_some() {
                non_empty += 1;
            }
        }
        assert_eq!(
            non_empty, 1,
            "all SAME-session calls must pin to a single shard"
        );
        let _ = drain.await;
    }
}
