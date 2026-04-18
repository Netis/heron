//! Failing reproductions for the out-of-order arrival bugs catalogued in
//! `docs/design/04b-turn-reorder-proposal.md` (§1, scenarios A–F).
//!
//! These tests are deliberately RED on `main`. They encode the intended
//! post-fix behavior; running them today demonstrates each bug concretely.
//!
//! Once the buffer-and-finalize machinery in 04b lands (Commit 2 of the
//! migration plan), these will be ported to the new `ingest(IdentifiedCall)`
//! API and turn green.

use std::net::IpAddr;
use std::sync::Arc;

use ts_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
use ts_llm::model::{
    ApiType, CallIdentity, FinishReason, IdentifiedCall, LlmCall, ProviderFormat,
};
use ts_llm::profiles;
use ts_turn::tracker::{TrackerConfig, TurnEvent, TurnTracker};
use ts_turn::{LlmTurn, TurnStatus};

// -------- helpers --------

fn test_metrics() -> MetricsWorker {
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker(
        "reorder-test",
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
    let _svc = sys.start();
    w
}

fn mk_tracker() -> TurnTracker {
    TurnTracker::new(
        Arc::new(profiles::build_default_registry()),
        TrackerConfig::default(),
        test_metrics(),
    )
}

/// Anthropic call with a session header; `body_kind` selects user-start
/// (`"text"`) vs continuation (`"tool_result"`). Optional `tools` makes the
/// call look like a sub-agent or main-agent context to claude-cli's profile.
fn anthropic_call(
    session: &str,
    request_time_us: i64,
    body_kind: &str,
    finish: FinishReason,
    tools: &[&str],
    response_text: Option<&str>,
) -> LlmCall {
    let body_inner = match body_kind {
        "text" => r#""content":[{"type":"text","text":"go"}]"#.to_string(),
        "tool_result" => {
            r#""content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]"#
                .to_string()
        }
        _ => unreachable!(),
    };
    let tools_json: String = tools
        .iter()
        .map(|t| format!(r#"{{"name":"{t}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        r#"{{"messages":[{{"role":"user",{body_inner}}}],"tools":[{tools_json}]}}"#
    );
    let response_body = response_text
        .map(|t| format!(r#"{{"content":[{{"type":"text","text":"{t}"}}]}}"#));

    LlmCall {
        stream_id: String::new(),
        id: format!("c-{request_time_us}"),
        provider: ProviderFormat::Anthropic,
        model: "claude".into(),
        api_type: ApiType::Chat,
        tenant_id: None,
        request_time: request_time_us,
        response_time: Some(request_time_us + 100_000),
        complete_time: Some(request_time_us + 200_000),
        request_path: "/v1/messages".into(),
        is_stream: true,
        request_body: Some(body),
        status_code: Some(200),
        finish_reason: Some(finish),
        response_body,
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
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
            ("User-Agent".into(), "claude-cli/2.1.98".into()),
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

/// Drain finalized turns from a slice of TurnEvents.
fn collect_turns(events: Vec<TurnEvent>) -> Vec<LlmTurn> {
    events
        .into_iter()
        .map(|TurnEvent::Completed(t)| t)
        .collect()
}

/// Drive a sequence of (call, identity) pairs through the tracker in the
/// given arrival order, then flush. Returns every Completed turn the
/// tracker produced.
fn run_in_order(t: &mut TurnTracker, sequence: Vec<(LlmCall, CallIdentity)>) -> Vec<LlmTurn> {
    let mut events = Vec::new();
    for (c, id) in sequence {
        events.extend(t.ingest(IdentifiedCall {
            call: Arc::new(c),
            identity: id,
        }));
    }
    events.extend(t.flush_all());
    collect_turns(events)
}

// -------- A: late user-start splits one turn into two --------

#[test]
fn bug_a_late_user_start_splits_turn() {
    // Logical turn: c1 (user-start, ToolUse) → c2 (tool_result, Complete).
    // c2 arrives first (different TCP, faster path); c1 arrives second.
    // After fix: one turn covering both calls, status Complete.
    // On `main`: split into two turns (one bare-c2 Complete, one bare-c1 Incomplete).
    let mut t = mk_tracker();
    let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse, &["Agent", "Bash"], None);
    let c2 = anthropic_call(
        "S",
        2_000_000,
        "tool_result",
        FinishReason::Complete,
        &["Agent", "Bash"],
        Some("done"),
    );
    let turns = run_in_order(
        &mut t,
        vec![(c2.clone(), id_for("S")), (c1.clone(), id_for("S"))],
    );

    assert_eq!(
        turns.len(),
        1,
        "out-of-order arrival of two same-turn calls must yield a single turn, got {}",
        turns.len()
    );
    let turn = &turns[0];
    assert_eq!(turn.status, TurnStatus::Complete);
    assert_eq!(turn.call_count, 2);
    assert_eq!(turn.call_ids, vec![c1.id.clone(), c2.id.clone()]);
}

// -------- B: 3-call reorder with the terminal in the middle --------

#[test]
fn bug_b_state_corruption_when_terminal_arrives_before_predecessors() {
    // Logical turn: c1 (user, ToolUse) → c2 (cont, ToolUse) → c3 (cont, Complete).
    // Arrival: c3, c1, c2.
    // After fix: one turn [c1,c2,c3], Complete, 3 calls.
    // On `main`: c3 starts its own turn; c1/c2 land in a separate orphan turn.
    let mut t = mk_tracker();
    let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse, &["Agent", "Bash"], None);
    let c2 = anthropic_call(
        "S",
        2_000_000,
        "tool_result",
        FinishReason::ToolUse,
        &["Agent", "Bash"],
        None,
    );
    let c3 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        FinishReason::Complete,
        &["Agent", "Bash"],
        Some("final"),
    );
    let turns = run_in_order(
        &mut t,
        vec![
            (c3.clone(), id_for("S")),
            (c1.clone(), id_for("S")),
            (c2.clone(), id_for("S")),
        ],
    );

    assert_eq!(
        turns.len(),
        1,
        "deeply reordered same-session calls must collapse into one turn"
    );
    let turn = &turns[0];
    assert_eq!(turn.status, TurnStatus::Complete);
    assert_eq!(turn.call_count, 3);
    assert_eq!(turn.call_ids, vec![c1.id.clone(), c2.id.clone(), c3.id.clone()]);
}

// -------- C: final_call_id / final_answer / last_activity track request_time --------

#[test]
fn bug_c_final_call_id_and_last_activity_track_request_time_not_arrival() {
    // c1 (ts=1s, user, ToolUse, response="welcome")
    // c2 (ts=2s, cont, ToolUse,  response="step 2")
    // c3 (ts=3s, cont, Complete, response="final")
    // Arrival: c3, c1, c2 — same arrival order as B but with response bodies
    // chosen so we can pin which call the turn's "final" fields point at.
    let mut t = mk_tracker();
    let c1 = anthropic_call(
        "S",
        1_000_000,
        "text",
        FinishReason::ToolUse,
        &["Agent", "Bash"],
        Some("welcome"),
    );
    let c2 = anthropic_call(
        "S",
        2_000_000,
        "tool_result",
        FinishReason::ToolUse,
        &["Agent", "Bash"],
        Some("step 2"),
    );
    let c3 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        FinishReason::Complete,
        &["Agent", "Bash"],
        Some("final"),
    );
    let turns = run_in_order(
        &mut t,
        vec![
            (c3.clone(), id_for("S")),
            (c1.clone(), id_for("S")),
            (c2.clone(), id_for("S")),
        ],
    );

    assert_eq!(turns.len(), 1, "expected single merged turn");
    let turn = &turns[0];
    assert_eq!(
        turn.final_call_id.as_deref(),
        Some(c3.id.as_str()),
        "final_call_id must point at the call with the largest request_time, not the latest arrival"
    );
    assert_eq!(turn.final_answer_preview.as_deref(), Some("final"));
    assert_eq!(
        turn.user_call_id.as_deref(),
        Some(c1.id.as_str()),
        "user_call_id must point at the user-start call regardless of arrival order"
    );
    assert_eq!(turn.user_input_preview.as_deref(), Some("go"));
    assert_eq!(
        turn.end_time_us,
        c3.complete_time.unwrap(),
        "end_time_us must derive from the latest-by-request_time call"
    );
}

// -------- D: late call after a turn finalized must not create a phantom turn --

#[test]
fn bug_d_late_call_after_finalize_is_orphan_not_phantom() {
    // First, finalize a clean two-call turn in order (c1, c2).
    // Then a straggler c0 (request_time strictly older than c1) arrives.
    // After fix: c0 is dropped as an orphan (it predates the high-water of
    // the finalized turn) — total turns produced = 1.
    // On `main`: c0 opens a brand-new ActiveTurn that idles out as Incomplete,
    // producing a second phantom turn.
    let mut t = mk_tracker();
    let c1 = anthropic_call(
        "S",
        2_000_000,
        "text",
        FinishReason::ToolUse,
        &["Agent", "Bash"],
        None,
    );
    let c2 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        FinishReason::Complete,
        &["Agent", "Bash"],
        Some("done"),
    );
    let c0 = anthropic_call(
        "S",
        1_000_000, // older than c1
        "text",
        FinishReason::ToolUse,
        &["Agent", "Bash"],
        None,
    );

    let mut events = Vec::new();
    events.extend(t.ingest(IdentifiedCall {
        call: Arc::new(c1.clone()),
        identity: id_for("S"),
    }));
    events.extend(t.ingest(IdentifiedCall {
        call: Arc::new(c2.clone()),
        identity: id_for("S"),
    }));
    // Push virtual time past c2's grace so the [c1,c2] partition finalizes
    // and the high-water mark gets recorded; only then can c0 be orphaned.
    events.extend(t.advance_time(
        c2.complete_time.unwrap() + TrackerConfig::default().grace_us + 1,
    ));
    events.extend(t.ingest(IdentifiedCall {
        call: Arc::new(c0.clone()),
        identity: id_for("S"),
    }));
    events.extend(t.flush_all());
    let turns = collect_turns(events);

    assert_eq!(
        turns.len(),
        1,
        "late call older than the finalized high-water must be dropped, not start a phantom turn"
    );
    let turn = &turns[0];
    assert_eq!(turn.status, TurnStatus::Complete);
    assert_eq!(turn.call_ids, vec![c1.id.clone(), c2.id.clone()]);
}

// -------- E: a partition with no user_turn_start is discarded -----------

#[test]
fn bug_e_partition_without_user_start_is_discarded() {
    // A lone continuation arrives — its leading user-start was never
    // observed (lost packet, missing capture window, sub-agent leftover
    // from a parent we never saw). The continuation even carries a
    // terminal finish_reason, so today's tracker emits it as a phantom
    // single-call turn.
    //
    // After 04b's discard rule (resolved decision #7): a partition that
    // contains zero `is_user_turn_start = Some(true)` calls is dropped
    // and counted via `TurnDiscardedNoUserStart`. Expected: zero turns.
    let mut t = mk_tracker();
    let c_orphan = anthropic_call(
        "S",
        1_000_000,
        "tool_result", // continuation body, is_user_turn_start = Some(false)
        FinishReason::Complete,
        &["Agent", "Bash"],
        Some("answer"),
    );
    let mut events = Vec::new();
    events.extend(t.ingest(IdentifiedCall {
        call: Arc::new(c_orphan),
        identity: id_for("S"),
    }));
    events.extend(t.flush_all());
    let turns = collect_turns(events);
    assert!(
        turns.is_empty(),
        "a partition with no user_turn_start must be discarded, got {} turn(s)",
        turns.len()
    );
}

// -------- F: heartbeat advances time, late call must still be orphaned --------

#[test]
fn bug_f_heartbeat_advance_does_not_open_phantom_for_late_call() {
    // Cleanly finalize a turn, then push virtual_now far ahead via a
    // heartbeat-style advance_time. A subsequent late call (request_time
    // older than the finalized high-water) must NOT create a new turn.
    let mut t = mk_tracker();
    let c1 = anthropic_call(
        "S",
        2_000_000,
        "text",
        FinishReason::ToolUse,
        &["Agent", "Bash"],
        None,
    );
    let c2 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        FinishReason::Complete,
        &["Agent", "Bash"],
        Some("done"),
    );

    let mut events = Vec::new();
    events.extend(t.ingest(IdentifiedCall {
        call: Arc::new(c1.clone()),
        identity: id_for("S"),
    }));
    events.extend(t.ingest(IdentifiedCall {
        call: Arc::new(c2.clone()),
        identity: id_for("S"),
    }));
    // Heartbeat well past the finalized turn.
    events.extend(t.advance_time(60_000_000));

    // Now the straggler shows up.
    let c0 = anthropic_call(
        "S",
        1_000_000,
        "text",
        FinishReason::ToolUse,
        &["Agent", "Bash"],
        None,
    );
    events.extend(t.ingest(IdentifiedCall {
        call: Arc::new(c0),
        identity: id_for("S"),
    }));
    events.extend(t.flush_all());

    let turns = collect_turns(events);
    assert_eq!(
        turns.len(),
        1,
        "heartbeat-advanced clock plus a late call must not create a phantom turn"
    );
    let turn = &turns[0];
    assert_eq!(turn.status, TurnStatus::Complete);
    assert_eq!(turn.call_ids, vec![c1.id.clone(), c2.id.clone()]);
}
