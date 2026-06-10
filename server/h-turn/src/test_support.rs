//! Test helpers shared between unit tests, integration tests, and downstream
//! crates. Construct minimal `AgentCall` values that bypass profile lookup so
//! tests can drive `TurnTracker` rollups with explicit per-call agent fields.

use std::sync::Arc;
use std::time::Duration;

use h_common::agent::ToolSurface;
use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
use h_llm::model::{ApiType, LlmCall};
use h_llm::wire_apis as wa;

use crate::tracker::{TrackerConfig, TurnEvent, TurnTracker};
use crate::AgentTurn;

pub use h_llm::model::{AgentCall, AgentCallInfo};

fn rollup_metrics() -> MetricsWorker {
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker(
        "test-rollup",
        &[
            Metric::TurnCallsIngested,
            Metric::TurnCallsAuxiliary,
            Metric::TurnsCompleted,
            Metric::TurnCallsDroppedLate,
            Metric::TurnClosedByGrace,
            Metric::TurnClosedByIdle,
            Metric::TurnDiscardedNoUserStart,
            Metric::TurnHeartbeatsReceived,
        ],
    );
    let _svc = sys.start();
    w
}

/// Build a minimal `AgentCall` whose per-call agent fields can be inspected by
/// the rollup. Surface, tool count and tool names are wired straight from the
/// args. Session id, user-start and terminal flags are filled in later by
/// [`feed_session_and_finalize`].
pub fn make_call(
    id: &str,
    tool_surface: Option<ToolSurface>,
    tool_call_count: u32,
    tool_names: &[&str],
) -> AgentCall {
    let request_time = 1_000_000_000i64; // overwritten in feed_session_and_finalize
    let call = LlmCall {
        source_id: String::new(),
        id: id.to_string(),
        wire_api: wa::ANTHROPIC,
        model: "test-model".into(),
        api_type: ApiType::Chat,
        request_time,
        response_time: Some(request_time + 100_000),
        complete_time: Some(request_time + 200_000),
        request_path: "/v1/messages".into(),
        is_stream: false,
        request_body: None,
        status_code: Some(200),
        finish_reason: Some("end_turn".into()),
        response_body: None,
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        ttft_ms: None,
        e2e_latency_ms: None,
        client_ip: "127.0.0.1".parse().unwrap(),
        client_port: 0,
        server_ip: "127.0.0.1".parse().unwrap(),
        server_port: 0,
        response_id: None,
        request_headers: vec![],
        response_headers: vec![],
        is_agent_request: tool_surface.is_some(),
        tool_surface,
        agent_topology: None,
        tool_call_count,
        tool_names: tool_names.iter().map(|s| s.to_string()).collect(),
        body_bytes_dropped: 0,
        process: None,
    };
    let agent = AgentCallInfo {
        agent_kind: "test",
        session_id: String::new(),
        subagent_name: None,
        is_user_turn_start: Some(false),
        is_turn_terminal: false,
        is_auxiliary: false,
        user_input: None,
        assistant_text: None,
        is_agent_request: tool_surface.is_some(),
        tool_surface,
        agent_topology: None,
        tool_names: tool_names.iter().map(|s| s.to_string()).collect(),
        tool_call_count,
        suspicious_signals: vec![],
    };
    AgentCall {
        call: Arc::new(call),
        agent,
    }
}

/// Feed `calls` through a `TurnTracker` (with zero grace) and return the
/// single finalized `AgentTurn`. The first call is forced into the user-start
/// role and the last into the terminal role so the partition closes exactly
/// once, no matter how the caller built each `AgentCall`.
pub fn feed_session_and_finalize(session_id: &str, calls: &[AgentCall]) -> AgentTurn {
    assert!(
        !calls.is_empty(),
        "need at least one call to finalize a turn"
    );
    let mut tracker = TurnTracker::new(
        TrackerConfig {
            grace: Duration::ZERO,
            ..TrackerConfig::default()
        },
        rollup_metrics(),
    );

    let n = calls.len();
    let base_request_time = 1_000_000_000i64;
    let mut events = Vec::new();
    for (idx, original) in calls.iter().enumerate() {
        let mut ic = original.clone();
        let mut call = (*ic.call).clone();
        call.request_time = base_request_time + idx as i64 * 1_000_000;
        call.response_time = Some(call.request_time + 100_000);
        call.complete_time = Some(call.request_time + 200_000);
        ic.call = Arc::new(call);
        ic.agent.session_id = session_id.to_string();
        ic.agent.is_user_turn_start = Some(idx == 0);
        ic.agent.is_turn_terminal = idx == n - 1;
        events.extend(tracker.ingest(ic));
    }
    events.extend(tracker.flush_all());

    let mut turns: Vec<AgentTurn> = events
        .into_iter()
        .map(|TurnEvent::Completed(t)| t)
        .collect();
    assert_eq!(
        turns.len(),
        1,
        "feed_session_and_finalize expected exactly one finalized turn, got {}",
        turns.len()
    );
    turns.pop().unwrap()
}
