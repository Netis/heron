//! Reorder / watermark regressions for `TurnTracker`.
//!
//! - Cases A–F cover the out-of-order arrival scenarios catalogued in
//!   `docs/design/04-turn.md` ("Motivation"). They started life as failing
//!   reproductions and are now green under buffer-and-finalize.
//! - Cases F1–F3 guard the two-clock split (per-source event-time watermark
//!   + wall-clock grace) against the three collapse modes it replaces.
//! - `finalize_by_grace_counter_does_not_double_count_*` pins the fix that
//!   made `emit_or_discard` return whether it actually emitted, so
//!   `TurnClosedByGrace` reflects emissions rather than `events.last()`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
use h_llm::agents;
use h_llm::model::{AgentCall, ApiType, LlmCall};
use h_llm::wire_apis as wa;
use h_turn::tracker::{TrackerConfig, TurnEvent, TurnTracker};
use h_turn::{AgentTurn, TurnStatus};

// -------- helpers --------

fn test_metrics() -> MetricsWorker {
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker(
        "reorder-test",
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

fn llm_test_metrics() -> MetricsWorker {
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker(
        "reorder-test-llm",
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

fn mk_tracker() -> TurnTracker {
    TurnTracker::new(TrackerConfig::default(), test_metrics())
}

/// Anthropic call with a session header; `body_kind` selects user-start
/// (`"text"`) vs continuation (`"tool_result"`). Optional `tools` makes the
/// call look like a sub-agent or main-agent context to claude-cli's profile.
fn anthropic_call(
    session: &str,
    request_time_us: i64,
    body_kind: &str,
    finish: &str,
    tools: &[&str],
    response_text: Option<&str>,
) -> LlmCall {
    let body_inner = match body_kind {
        "text" => r#""content":[{"type":"text","text":"go"}]"#.to_string(),
        "tool_result" => {
            r#""content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]"#.to_string()
        }
        _ => unreachable!(),
    };
    let tools_json: String = tools
        .iter()
        .map(|t| format!(r#"{{"name":"{t}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(r#"{{"messages":[{{"role":"user",{body_inner}}}],"tools":[{tools_json}]}}"#);
    let response_body =
        response_text.map(|t| format!(r#"{{"content":[{{"type":"text","text":"{t}"}}]}}"#));

    LlmCall {
        source_id: String::new(),
        id: format!("c-{request_time_us}"),
        wire_api: wa::ANTHROPIC,
        model: "claude".into(),
        api_type: ApiType::Chat,
        request_time: request_time_us,
        response_time: Some(request_time_us + 100_000),
        complete_time: Some(request_time_us + 200_000),
        request_path: "/v1/messages".into(),
        is_stream: true,
        request_body: Some(body),
        status_code: Some(200),
        finish_reason: Some(finish.to_string()),
        response_body,
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
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
            ("User-Agent".into(), "claude-cli/2.1.98".into()),
            ("X-Claude-Code-Session-Id".into(), session.into()),
        ],
        response_headers: vec![],
        is_agent_request: false,
        tool_surface: None,
        agent_topology: None,
        tool_call_count: 0,
        tool_names: vec![],
        body_bytes_dropped: 0,
    }
}

/// Build an `AgentCall` by running the production call-info pipeline against
/// `call`. Replaces the previous pattern of constructing a stub `AgentCallInfo`
/// by hand: classification fields (`is_turn_terminal`, `is_user_turn_start`,
/// `subagent_name`, ...) now live on `AgentCallInfo` and the tracker reads
/// them, so tests must populate them the way the real pipeline would.
fn agent_call(call: LlmCall) -> AgentCall {
    let reg = agents::build_default_registry();
    let wa_reg = h_llm::wire_apis::build_default_wire_api_registry();
    let metrics = llm_test_metrics();
    let agent = h_llm::build_agent_call_info(
        &call,
        &reg,
        &wa_reg,
        &h_llm::agent_classifier::ClassifierConfig::default(),
        &metrics,
    )
    .expect("call info");
    AgentCall {
        call: Arc::new(call),
        agent,
    }
}

/// Drain finalized turns from a slice of TurnEvents.
fn collect_turns(events: Vec<TurnEvent>) -> Vec<AgentTurn> {
    events
        .into_iter()
        .map(|TurnEvent::Completed(t)| t)
        .collect()
}

/// Drive a sequence of calls through the tracker in the given arrival order,
/// then flush. Identity is built per-call via the production pipeline (see
/// [`agent_call`]). Returns every Completed turn the tracker produced.
fn run_in_order(t: &mut TurnTracker, sequence: Vec<LlmCall>) -> Vec<AgentTurn> {
    let mut events = Vec::new();
    for c in sequence {
        events.extend(t.ingest(agent_call(c)));
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
    let c1 = anthropic_call("S", 1_000_000, "text", "tool_use", &["Agent", "Bash"], None);
    let c2 = anthropic_call(
        "S",
        2_000_000,
        "tool_result",
        "end_turn",
        &["Agent", "Bash"],
        Some("done"),
    );
    let turns = run_in_order(&mut t, vec![c2.clone(), c1.clone()]);

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
    let c1 = anthropic_call("S", 1_000_000, "text", "tool_use", &["Agent", "Bash"], None);
    let c2 = anthropic_call(
        "S",
        2_000_000,
        "tool_result",
        "tool_use",
        &["Agent", "Bash"],
        None,
    );
    let c3 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        "end_turn",
        &["Agent", "Bash"],
        Some("final"),
    );
    let turns = run_in_order(&mut t, vec![c3.clone(), c1.clone(), c2.clone()]);

    assert_eq!(
        turns.len(),
        1,
        "deeply reordered same-session calls must collapse into one turn"
    );
    let turn = &turns[0];
    assert_eq!(turn.status, TurnStatus::Complete);
    assert_eq!(turn.call_count, 3);
    assert_eq!(
        turn.call_ids,
        vec![c1.id.clone(), c2.id.clone(), c3.id.clone()]
    );
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
        "tool_use",
        &["Agent", "Bash"],
        Some("welcome"),
    );
    let c2 = anthropic_call(
        "S",
        2_000_000,
        "tool_result",
        "tool_use",
        &["Agent", "Bash"],
        Some("step 2"),
    );
    let c3 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        "end_turn",
        &["Agent", "Bash"],
        Some("final"),
    );
    let turns = run_in_order(&mut t, vec![c3.clone(), c1.clone(), c2.clone()]);

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
    let c1 = anthropic_call("S", 2_000_000, "text", "tool_use", &["Agent", "Bash"], None);
    let c2 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        "end_turn",
        &["Agent", "Bash"],
        Some("done"),
    );
    let c0 = anthropic_call(
        "S",
        1_000_000, // older than c1
        "text",
        "tool_use",
        &["Agent", "Bash"],
        None,
    );

    // Wall-clock-paced ingest: start at t0, push each step past the default
    // grace (1 s) so the [c1,c2] partition finalizes and the high-water mark
    // gets recorded before c0 shows up.
    let t0 = Instant::now();
    let past_grace = t0 + TrackerConfig::default().grace + Duration::from_micros(1);
    let mut events = Vec::new();
    events.extend(t.ingest_at(agent_call(c1.clone()), t0));
    events.extend(t.ingest_at(agent_call(c2.clone()), t0));
    events.extend(t.advance_time_at(c2.complete_time.unwrap() + 1, &c2.source_id, past_grace));
    events.extend(t.ingest_at(agent_call(c0.clone()), past_grace));
    events.extend(t.flush_all_at(past_grace));
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
        "end_turn",
        &["Agent", "Bash"],
        Some("answer"),
    );
    let mut events = Vec::new();
    events.extend(t.ingest(agent_call(c_orphan)));
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
    // Cleanly finalize a turn, then push the source's event-time watermark
    // far ahead via advance_time (plus wall-clock past grace). A subsequent
    // late call (request_time older than the finalized high-water) must NOT
    // create a new turn — the orphan guard on last_finalized_request_time
    // catches it.
    let mut t = mk_tracker();
    let c1 = anthropic_call("S", 2_000_000, "text", "tool_use", &["Agent", "Bash"], None);
    let c2 = anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        "end_turn",
        &["Agent", "Bash"],
        Some("done"),
    );

    let t0 = Instant::now();
    let past_grace = t0 + TrackerConfig::default().grace + Duration::from_micros(1);
    let mut events = Vec::new();
    events.extend(t.ingest_at(agent_call(c1.clone()), t0));
    events.extend(t.ingest_at(agent_call(c2.clone()), t0));
    // Heartbeat well past the finalized turn (wall-clock drives grace).
    events.extend(t.advance_time_at(60_000_000, &c2.source_id, past_grace));

    // Now the straggler shows up.
    let c0 = anthropic_call("S", 1_000_000, "text", "tool_use", &["Agent", "Bash"], None);
    events.extend(t.ingest_at(agent_call(c0), past_grace));
    events.extend(t.flush_all_at(past_grace));

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

// -------- watermark isolation regressions (F1/F2/F3) --------------------

/// Anthropic call with explicit `source_id` — the default `anthropic_call`
/// hard-codes an empty `source_id`, but the watermark-isolation tests need
/// multiple sources.
fn anthropic_call_on_source(
    source: &str,
    session: &str,
    request_time_us: i64,
    body_kind: &str,
    finish: &str,
    tools: &[&str],
    response_text: Option<&str>,
) -> LlmCall {
    let mut c = anthropic_call(
        session,
        request_time_us,
        body_kind,
        finish,
        tools,
        response_text,
    );
    c.source_id = source.to_string();
    c
}

fn id_counter() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    use std::sync::OnceLock;
    static N: OnceLock<AtomicU64> = OnceLock::new();
    N.get_or_init(|| AtomicU64::new(0))
}

fn unique_call(mut c: LlmCall) -> LlmCall {
    let n = id_counter().fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    c.id = format!("c-{}-{}", c.request_time, n);
    c
}

// -------- F1: cross-flow-worker HB within one source must not orphan a
// same-session laggard whose processing path happens to be slower -------------

#[test]
fn f1_intra_source_hb_does_not_orphan_same_session_laggard() {
    // Scenario: session S spans two TCP connections that hash to different
    // flow-workers in the same source. Fast worker Y forwards its HBs to the
    // turn-shard ahead of slow worker X's still-in-flight call. Under an
    // event-time grace, Y's HB would fast-forward the session's grace
    // timeline past `terminal.arrived_at + grace` and finalize before X's
    // laggard ever arrived — turning the laggard into a TurnCallsDroppedLate
    // and splitting one turn into two.
    //
    // Under the wall-clock grace in place, HBs only bump the per-source
    // event-time watermark; grace only fires when wall-clock moves. As long
    // as ingest / advance_time happen at the same `Instant`, the laggard
    // joins the turn.
    let mut t = mk_tracker();

    // Terminal T (user-start + Complete) already lands.
    let terminal = unique_call(anthropic_call_on_source(
        "sA",
        "S",
        10_000_000,
        "text",
        "end_turn",
        &["Agent", "Bash"],
        Some("final"),
    ));
    // Laggard L (continuation) with earlier request_time — it arrives *after*
    // an aggressive heartbeat tries to fast-forward the clock.
    let laggard = unique_call(anthropic_call_on_source(
        "sA",
        "S",
        5_000_000,
        "tool_result",
        "tool_use",
        &["Agent", "Bash"],
        None,
    ));

    let t0 = Instant::now();
    let mut events = Vec::new();

    // Terminal arrives first at the turn-shard.
    events.extend(t.ingest_at(agent_call(terminal.clone()), t0));

    // Aggressive HB from a fast worker on the same source — event-time jumps
    // far past any reasonable event-time grace. Wall-clock stays at t0.
    events.extend(t.advance_time_at(terminal.complete_time.unwrap() + 60_000_000, "sA", t0));

    // Laggard shows up "next nanosecond" in wall-clock.
    events.extend(t.ingest_at(agent_call(laggard.clone()), t0));

    // Only after the wall-clock grace has actually elapsed should the turn
    // finalize — and now the laggard is part of it.
    let past_grace = t0 + TrackerConfig::default().grace + Duration::from_micros(1);
    events.extend(t.flush_all_at(past_grace));

    let turns = collect_turns(events);
    assert_eq!(
        turns.len(),
        1,
        "laggard must join the same turn, not be orphaned; got {} turn(s)",
        turns.len()
    );
    assert_eq!(
        turns[0].call_count, 2,
        "terminal + laggard expected — orphaned implies grace was event-time fast-forwarded"
    );
    let mut ids = turns[0].call_ids.clone();
    ids.sort();
    let mut want = vec![terminal.id.clone(), laggard.id.clone()];
    want.sort();
    assert_eq!(ids, want);
}

// -------- F2: a busy *other* session must not shrink this session's grace ----

#[test]
fn f2_cross_session_ingest_does_not_collapse_other_session_grace() {
    // Session A has a pending terminal; grace has NOT yet elapsed by wall-clock.
    // Session B ingests a brand-new call whose event-time is far in the future
    // — enough that a shared (non-per-source, event-time) watermark would have
    // jumped past `A.terminal.arrived + grace` and finalized A prematurely.
    // Then session A receives a same-turn laggard.
    //
    // Expected: A finalizes as one turn with both calls, not split.
    let mut t = mk_tracker();

    let a_terminal = unique_call(anthropic_call_on_source(
        "sA",
        "A",
        10_000_000,
        "text",
        "end_turn",
        &["Agent", "Bash"],
        Some("a-final"),
    ));
    let a_laggard = unique_call(anthropic_call_on_source(
        "sA",
        "A",
        5_000_000,
        "tool_result",
        "tool_use",
        &["Agent", "Bash"],
        None,
    ));
    // Session B on the SAME source, with a wildly newer event-time.
    let b_call = unique_call(anthropic_call_on_source(
        "sA",
        "B",
        10_000_000_000, // 10^10 us = 10_000 s
        "text",
        "tool_use",
        &["Agent", "Bash"],
        None,
    ));

    let t0 = Instant::now();
    let mut events = Vec::new();

    events.extend(t.ingest_at(agent_call(a_terminal.clone()), t0));
    // Busy session B ingests with a huge event-time at the same wall-clock.
    events.extend(t.ingest_at(agent_call(b_call), t0));
    // A's laggard comes in right after — wall-clock still t0.
    events.extend(t.ingest_at(agent_call(a_laggard.clone()), t0));

    // Nothing should have finalized yet: wall-clock has not moved past grace.
    assert!(
        collect_turns(events.clone()).is_empty(),
        "no turn should finalize before wall-clock grace elapses"
    );

    // Now wall-clock crosses grace; finalize A.
    events.extend(t.flush_all_at(t0 + TrackerConfig::default().grace + Duration::from_micros(1)));
    let turns: Vec<_> = collect_turns(events)
        .into_iter()
        .filter(|t| t.session_id == "A")
        .collect();
    assert_eq!(turns.len(), 1, "A must end up as exactly one turn");
    assert_eq!(
        turns[0].call_count, 2,
        "A's terminal + laggard must bundle, not split — got {} calls",
        turns[0].call_count
    );
}

// -------- F3: one source's heartbeat must not trigger finalize in another ----

#[test]
fn f3_cross_source_hb_does_not_expire_other_source_grace() {
    // Stream sA has a terminal pending. Stream sB heartbeats aggressively —
    // its ts alone would have shoved a global event-time watermark past
    // `sA.terminal.arrived + grace` and finalized sA.
    //
    // Per-source watermark: sB's HB only bumps sB. sA's wall-clock also
    // hasn't crossed grace. Therefore sA must still be pending.
    let mut t = mk_tracker();

    let a_terminal = unique_call(anthropic_call_on_source(
        "sA",
        "SA",
        10_000_000,
        "text",
        "end_turn",
        &["Agent", "Bash"],
        Some("a-final"),
    ));

    let t0 = Instant::now();
    let mut events = Vec::new();

    events.extend(t.ingest_at(agent_call(a_terminal.clone()), t0));

    // Stream sB heartbeats far into its own future. Same wall-clock.
    for ts in [20_000_000, 40_000_000, 80_000_000, 160_000_000] {
        events.extend(t.advance_time_at(ts, "sB", t0));
    }
    assert!(
        collect_turns(events.clone()).is_empty(),
        "sB heartbeats must not finalize sA's pending turn"
    );
    assert_eq!(
        t.active_count(),
        1,
        "sA's terminal should still be pending after unrelated-source heartbeats"
    );

    // Wall-clock crosses grace → sA finalizes.
    events.extend(t.flush_all_at(t0 + TrackerConfig::default().grace + Duration::from_micros(1)));
    let turns = collect_turns(events);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].session_id, "SA");
    assert_eq!(turns[0].call_count, 1);
}

// -------- grace counter double-count regression ------------------------------

#[test]
fn finalize_by_grace_counter_does_not_double_count_across_discarded_partition() {
    // Build a buffer with TWO terminals: the first partition has a
    // user_turn_start and should EMIT; the second partition is a lone
    // continuation terminal with NO user_turn_start and must be DISCARDED.
    //
    // Before the fix, finalize_session peeked at `events.last()` after each
    // partition. Once the first partition had pushed a Completed, the
    // discarded second partition would still see Completed as the last event
    // and bump TurnClosedByGrace a second time. Expected: exactly one
    // grace finalization.
    use h_turn::tracker::{TrackerConfig, TurnTracker};

    // Build the tracker with a shared metrics worker we can read back.
    let metrics = test_metrics();
    let mut t = TurnTracker::new(TrackerConfig::default(), metrics.clone());

    // Partition A: user-start + continuation-terminal.
    let c_user = unique_call(anthropic_call(
        "S",
        1_000_000,
        "text",
        "tool_use",
        &["Agent", "Bash"],
        None,
    ));
    let c_term_a = unique_call(anthropic_call(
        "S",
        2_000_000,
        "tool_result",
        "end_turn",
        &["Agent", "Bash"],
        Some("a-done"),
    ));
    // Partition B: lone continuation-terminal with NO user_turn_start.
    let c_term_b = unique_call(anthropic_call(
        "S",
        3_000_000,
        "tool_result",
        "end_turn",
        &["Agent", "Bash"],
        Some("b-done"),
    ));

    let t0 = Instant::now();
    let mut events = Vec::new();
    events.extend(t.ingest_at(agent_call(c_user), t0));
    events.extend(t.ingest_at(agent_call(c_term_a), t0));
    events.extend(t.ingest_at(agent_call(c_term_b), t0));
    // Push wall-clock past grace so finalize_session iterates both terminals.
    let past_grace = t0 + TrackerConfig::default().grace + Duration::from_micros(1);
    events.extend(t.flush_all_at(past_grace));

    let turns = collect_turns(events);
    assert_eq!(
        turns.len(),
        1,
        "exactly one turn should survive discard rule"
    );
    assert_eq!(turns[0].call_count, 2);

    let grace = metrics.counter(Metric::TurnClosedByGrace).get();
    let discarded = metrics.counter(Metric::TurnDiscardedNoUserStart).get();
    assert_eq!(
        grace, 1,
        "one partition actually finalized by grace; got {grace} (the old bug double-counted)"
    );
    assert_eq!(
        discarded, 1,
        "the second partition is dropped by the discard rule"
    );
}
