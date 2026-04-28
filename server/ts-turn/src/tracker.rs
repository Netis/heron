//! Buffer-and-finalize turn tracker. See `docs/design/04-turn.md`.
//!
//! Each `(source_id, session_id)` owns a `SessionBuffer` that holds calls
//! sorted by `request_time` until a main-agent terminal call appears and its
//! grace window elapses. On grace expiry the buffer is partitioned at each
//! terminal and every partition becomes one `AgentTurn`. Partitions that
//! contain no `is_user_turn_start = Some(true)` call are discarded.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_llm::model::AgentCall;
use ts_llm::profile::{AgentProfile, AgentProfileRegistry};
use ts_llm::wire_api_registry::WireApiRegistry;

use crate::model::{AgentTurn, TurnStatus};

const FINAL_ANSWER_PREVIEW_CHARS: usize = 500;
const USER_INPUT_PREVIEW_CHARS: usize = 500;

fn truncate_preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Tracker configuration. Two clocks:
/// * `idle_timeout_us` and `sweep_interval_us` are in **event-time**
///   microseconds (matching `LlmCall.request_time`), driven by per-source
///   watermarks so pcap replay still works.
/// * `grace` is **wall-clock**, measured via `Instant`. Grace is a pipeline
///   fan-in jitter budget — physical-time is the correct unit. Using
///   event-time here would let any other source's heartbeat or any other
///   session's ingest fast-forward an unrelated session's grace.
#[derive(Debug, Clone, Copy)]
pub struct TrackerConfig {
    pub idle_timeout_us: i64,
    pub sweep_interval_us: i64,
    /// Wall-clock wait after a terminal call lands before partitioning the
    /// session buffer. Covers fan-in jitter from same-session calls riding
    /// on different TCP connections / different llm-stage workers.
    pub grace: Duration,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_us: 600_000_000,        // 600 s event-time
            sweep_interval_us: 10_000_000,       // 10 s event-time
            grace: Duration::from_millis(1_000), // 1 s wall-clock
        }
    }
}

/// Tracker output. Only finalized turns are emitted; per-call lifecycle
/// events were removed when the buffer model replaced ActiveTurn (04b §6.4).
#[derive(Debug, Clone)]
pub enum TurnEvent {
    Completed(AgentTurn),
}

#[derive(Debug)]
struct BufferedCall {
    ic: AgentCall,
    /// Wall-clock `Instant` at the moment ingest stored this call. Each
    /// terminal's grace is checked against its own arrival time **in
    /// wall-clock**, not event time — grace measures pipeline fan-in jitter,
    /// which is a physical-time phenomenon.
    arrived_at_wall: Instant,
    /// Cached `is_main_terminal` so finalize doesn't re-resolve per element.
    is_terminal: bool,
}

#[derive(Default, Debug)]
struct SessionBuffer {
    /// Calls awaiting partition, ordered by `request_time`. The Vec at each
    /// key handles request_time collisions in insertion order.
    pending: BTreeMap<i64, Vec<BufferedCall>>,
    /// `arrived_at_wall` of the earliest pending terminal. `None` ⇒ no
    /// terminal currently pending. Set by ingest; reseated by finalize after
    /// each partition emission.
    grace_started_at_wall: Option<Instant>,
    /// Largest `request_time` ever included in a finalized (or discarded)
    /// partition for this session. New arrivals strictly older than this are
    /// orphaned at the entry guard.
    last_finalized_request_time: Option<i64>,
    /// Latest per-source `virtual_now` observed for this buffer, in
    /// event-time µs. Used as the idle reference by `sweep` / `gc_buffers`.
    last_activity_us: i64,
}

/// Single stateful owner of turn assembly. Passive: callers drive it via
/// `ingest`, `advance_time`, `sweep`, `flush_all`.
///
/// Two clocks by design:
/// * `virtual_now_by_source` — per-source event-time watermark. Bumped by
///   each source's own heartbeats and ingested calls. Used for idle sweep,
///   gc, and the orphan guard. Per-source so one source's activity cannot
///   fast-forward another source's idle/gc horizon.
/// * `Instant` taken at ingest / advance — wall-clock. Used only for grace.
pub struct TurnTracker {
    registry: Arc<AgentProfileRegistry>,
    /// Wire-API registry, used by `is_main_terminal` to ask each call's
    /// wire_api whether its `finish_reason` is terminal (and not a tool_use).
    /// Keeping the predicate ownership on the wire-API trait means the tracker
    /// stays agnostic of provider-specific finish_reason vocabulary.
    wire_apis: Arc<WireApiRegistry>,
    config: TrackerConfig,
    buffers: HashMap<(String, String), SessionBuffer>,
    /// Event-time watermark, keyed by `source_id`. `0` ≡ "never seen".
    virtual_now_by_source: HashMap<String, i64>,
    /// Global sweep-interval throttle, in event-time µs. Not per-source — it
    /// only controls how often `sweep` iterates buffers, not correctness.
    last_sweep_us: i64,
    metrics: MetricsWorker,
}

impl TurnTracker {
    pub fn new(
        registry: Arc<AgentProfileRegistry>,
        wire_apis: Arc<WireApiRegistry>,
        config: TrackerConfig,
        metrics: MetricsWorker,
    ) -> Self {
        Self {
            registry,
            wire_apis,
            config,
            buffers: HashMap::new(),
            virtual_now_by_source: HashMap::new(),
            last_sweep_us: 0,
            metrics,
        }
    }

    /// Total pending calls across every buffer.
    pub fn active_count(&self) -> usize {
        self.buffers
            .values()
            .map(|b| b.pending.values().map(|v| v.len()).sum::<usize>())
            .sum()
    }

    /// Bump the per-source event-time watermark to `max(current, ts)` and
    /// return the new value.
    fn bump_event_time(&mut self, source_id: &str, ts: i64) -> i64 {
        let e = self
            .virtual_now_by_source
            .entry(source_id.to_string())
            .or_insert(0);
        *e = (*e).max(ts);
        *e
    }

    /// Ingest one agent-attributed, completed call using the current wall-clock.
    pub fn ingest(&mut self, ic: AgentCall) -> Vec<TurnEvent> {
        self.ingest_at(ic, Instant::now())
    }

    /// Ingest with an injectable wall-clock (tests).
    ///
    /// Event-time watermark is bumped on the **call's own `source_id`** only;
    /// grace timestamps are wall-clock. Together this ensures that no activity
    /// outside this `(source_id, session_id)` can shrink its grace window.
    pub fn ingest_at(&mut self, ic: AgentCall, now_wall: Instant) -> Vec<TurnEvent> {
        let arrival_ts = ic
            .call
            .complete_time
            .or(ic.call.response_time)
            .unwrap_or(ic.call.request_time);
        let source_id = ic.call.source_id.clone();
        let virtual_now = self.bump_event_time(&source_id, arrival_ts);
        self.metrics.counter(Metric::TurnCallsIngested).inc();

        let registry = Arc::clone(&self.registry);
        let profile = match registry.find_by_name(ic.agent.agent_kind) {
            Some(p) => p,
            None => return self.flush_ready_buffers(now_wall),
        };

        // Auxiliary one-shots (e.g., claude-cli session-title) bypass turn
        // assembly entirely. They still flow to storage independently.
        if profile.is_auxiliary(&ic.call) {
            self.metrics.counter(Metric::TurnCallsAuxiliary).inc();
            return self.flush_ready_buffers(now_wall);
        }

        let key = (source_id, ic.agent.session_id.clone());
        let buf = self.buffers.entry(key).or_default();

        if let Some(hw) = buf.last_finalized_request_time {
            if ic.call.request_time < hw {
                self.metrics.counter(Metric::TurnCallsDroppedLate).inc();
                return self.flush_ready_buffers(now_wall);
            }
        }

        let is_terminal = is_main_terminal(profile, &self.wire_apis, &ic);
        let request_time = ic.call.request_time;
        buf.pending
            .entry(request_time)
            .or_default()
            .push(BufferedCall {
                ic,
                arrived_at_wall: now_wall,
                is_terminal,
            });
        buf.last_activity_us = virtual_now;
        if is_terminal && buf.grace_started_at_wall.is_none() {
            buf.grace_started_at_wall = Some(now_wall);
        }

        self.flush_ready_buffers(now_wall)
    }

    /// Walk every buffer; for each whose front-pending terminal's grace has
    /// expired (by wall-clock), hand off to `finalize_session` to emit one or
    /// more turns.
    fn flush_ready_buffers(&mut self, now_wall: Instant) -> Vec<TurnEvent> {
        let grace = self.config.grace;
        let registry = Arc::clone(&self.registry);

        let ready_keys: Vec<(String, String)> = self
            .buffers
            .iter()
            .filter_map(|(k, b)| match b.grace_started_at_wall {
                Some(started) if now_wall.duration_since(started) >= grace => Some(k.clone()),
                _ => None,
            })
            .collect();

        let mut events = Vec::new();
        for key in ready_keys {
            let agent_kind = match self
                .buffers
                .get(&key)
                .and_then(|b| b.pending.values().flatten().next())
                .map(|bc| bc.ic.agent.agent_kind)
            {
                Some(n) => n,
                None => continue,
            };
            let profile = match registry.find_by_name(agent_kind) {
                Some(p) => p,
                None => continue,
            };
            let buf = self.buffers.get_mut(&key).expect("key just listed");
            finalize_session(
                profile,
                &self.wire_apis,
                &key.0,
                &key.1,
                buf,
                now_wall,
                grace,
                &self.metrics,
                &mut events,
            );
        }
        self.gc_buffers();
        events
    }

    /// Drop fully-drained buffers whose `last_activity_us` is well past the
    /// idle horizon (measured against **their own source's** virtual_now, so
    /// a quiet source's old buffers aren't gc'd because another source has
    /// moved on). Past `2 × idle_timeout_us` we lose the orphan guard for
    /// that session — far longer than any plausible reorder.
    fn gc_buffers(&mut self) {
        let cfg_idle = self.config.idle_timeout_us;
        let vn_by_source = &self.virtual_now_by_source;
        self.buffers.retain(|(source_id, _), b| {
            let vn = vn_by_source.get(source_id).copied().unwrap_or(0);
            let cutoff = vn.saturating_sub(2 * cfg_idle);
            !(b.pending.is_empty()
                && b.last_finalized_request_time.is_some()
                && b.last_activity_us < cutoff)
        });
    }

    /// Advance virtual time using a heartbeat forwarded through the pipeline.
    /// Bumps only the event-time watermark of **this source**; grace is
    /// evaluated against wall-clock, so a busy source cannot fast-forward a
    /// quiet source's grace window.
    pub fn advance_time(&mut self, ts: i64, source_id: &str) -> Vec<TurnEvent> {
        self.advance_time_at(ts, source_id, Instant::now())
    }

    /// Same as [`advance_time`], with an injectable wall-clock (tests).
    pub fn advance_time_at(
        &mut self,
        ts: i64,
        source_id: &str,
        now_wall: Instant,
    ) -> Vec<TurnEvent> {
        self.bump_event_time(source_id, ts);
        let mut events = self.flush_ready_buffers(now_wall);
        // Sweep is pure event-time logic — no wall-clock variant needed.
        events.extend(self.sweep());
        events
    }

    /// Idle fallback: drain buffers that hold no terminal call and whose
    /// newest call is older than `idle_timeout_us` in their own source's
    /// event-time. Discard rule applies.
    ///
    /// Event-time only — no wall-clock input. Callers with a wall-clock in
    /// hand (test-driven `advance_time_at`) don't need an `_at` variant
    /// because sweep never consults wall-clock.
    pub fn sweep(&mut self) -> Vec<TurnEvent> {
        // Throttle: use the max across all per-source watermarks. sweep only
        // runs at most once per `sweep_interval_us` of global progress — this
        // is a coarse rate limiter, not a correctness-critical clock.
        let global_virtual_now = self
            .virtual_now_by_source
            .values()
            .copied()
            .max()
            .unwrap_or(0);
        if global_virtual_now - self.last_sweep_us < self.config.sweep_interval_us {
            return Vec::new();
        }
        self.last_sweep_us = global_virtual_now;
        let cfg_idle = self.config.idle_timeout_us;
        let registry = Arc::clone(&self.registry);

        let candidates: Vec<(String, String)> = {
            let vn_by_source = &self.virtual_now_by_source;
            self.buffers
                .iter()
                .filter(|((source_id, _), b)| {
                    let vn = vn_by_source.get(source_id).copied().unwrap_or(0);
                    !b.pending.is_empty()
                        && !b.pending.values().flatten().any(|bc| bc.is_terminal)
                        && b.last_activity_us < vn - cfg_idle
                })
                .map(|(k, _)| k.clone())
                .collect()
        };

        let mut events = Vec::new();
        for key in candidates {
            let buf = match self.buffers.get_mut(&key) {
                Some(b) => b,
                None => continue,
            };
            let drained: Vec<BufferedCall> = std::mem::take(&mut buf.pending)
                .into_values()
                .flatten()
                .collect();
            if drained.is_empty() {
                continue;
            }
            let max_request_time = drained
                .iter()
                .map(|bc| bc.ic.call.request_time)
                .max()
                .expect("non-empty");
            buf.last_finalized_request_time = Some(max_request_time);
            buf.grace_started_at_wall = None;

            let agent_kind = drained[0].ic.agent.agent_kind;
            let profile = match registry.find_by_name(agent_kind) {
                Some(p) => p,
                None => continue,
            };

            // sweep already counts itself via FinalizeKind::Idle inside
            // emit_or_discard; the returned bool isn't needed here.
            let _ = emit_or_discard(
                profile,
                &self.wire_apis,
                &key.0,
                &key.1,
                &drained,
                &self.metrics,
                &mut events,
                FinalizeKind::Idle,
            );
        }
        self.gc_buffers();
        events
    }

    /// EOF / shutdown drain using the current wall-clock.
    pub fn flush_all(&mut self) -> Vec<TurnEvent> {
        self.flush_all_at(Instant::now())
    }

    /// EOF / shutdown drain with an injectable wall-clock (tests). Force
    /// every pending terminal past its grace, then emit any remaining
    /// non-terminal tail (subject to discard rule).
    pub fn flush_all_at(&mut self, now_wall: Instant) -> Vec<TurnEvent> {
        let registry = Arc::clone(&self.registry);
        let mut keys: Vec<(String, String)> = self.buffers.keys().cloned().collect();
        keys.sort();
        // Force all terminals past their grace by advancing wall-clock beyond
        // any pending terminal's arrival + grace. `grace + 1ns` off now_wall
        // is enough because new arrivals at/after `now_wall` can't be in the
        // buffer yet.
        let force_wall = now_wall
            .checked_add(self.config.grace)
            .and_then(|t| t.checked_add(Duration::from_nanos(1)))
            .unwrap_or(now_wall);
        let grace = self.config.grace;

        let mut events = Vec::new();
        for key in keys {
            let agent_kind = match self
                .buffers
                .get(&key)
                .and_then(|b| b.pending.values().flatten().next())
                .map(|bc| bc.ic.agent.agent_kind)
            {
                Some(n) => n,
                None => continue,
            };
            let profile = match registry.find_by_name(agent_kind) {
                Some(p) => p,
                None => continue,
            };

            // Step 1: drive the terminal-bounded partitions to finalize.
            {
                let buf = self.buffers.get_mut(&key).expect("listed above");
                finalize_session(
                    profile,
                    &self.wire_apis,
                    &key.0,
                    &key.1,
                    buf,
                    force_wall,
                    grace,
                    &self.metrics,
                    &mut events,
                );
            }
            // Step 2: drain any non-terminal tail as Incomplete (or discard).
            let buf = self.buffers.get_mut(&key).expect("listed above");
            let drained: Vec<BufferedCall> = std::mem::take(&mut buf.pending)
                .into_values()
                .flatten()
                .collect();
            if drained.is_empty() {
                continue;
            }
            let max_request_time = drained
                .iter()
                .map(|bc| bc.ic.call.request_time)
                .max()
                .expect("non-empty");
            buf.last_finalized_request_time = Some(max_request_time);
            buf.grace_started_at_wall = None;
            // flush_all's non-terminal tail doesn't have a dedicated counter
            // beyond TurnsCompleted (bumped inside emit_or_discard).
            let _ = emit_or_discard(
                profile,
                &self.wire_apis,
                &key.0,
                &key.1,
                &drained,
                &self.metrics,
                &mut events,
                FinalizeKind::Flush,
            );
        }
        events
    }
}

#[derive(Copy, Clone)]
enum FinalizeKind {
    Idle,
    Flush,
}

/// Loop-emit one turn per pending main-agent terminal whose own grace has
/// expired, partitioning at each terminal's `request_time`. Stops as soon
/// as the next pending terminal still sits inside its grace window, and
/// reseats `buf.grace_started_at_wall` to that next terminal's arrival.
#[allow(clippy::too_many_arguments)]
fn finalize_session(
    profile: &dyn AgentProfile,
    wire_apis: &WireApiRegistry,
    source_id: &str,
    session_id: &str,
    buf: &mut SessionBuffer,
    now_wall: Instant,
    grace: Duration,
    metrics: &MetricsWorker,
    events: &mut Vec<TurnEvent>,
) {
    loop {
        let mut sorted: Vec<&BufferedCall> = buf.pending.values().flatten().collect();
        sorted.sort_by_key(|bc| bc.ic.call.request_time);
        if sorted.is_empty() {
            buf.grace_started_at_wall = None;
            return;
        }

        let terminal_idx = sorted.iter().position(|bc| bc.is_terminal);
        let idx = match terminal_idx {
            None => {
                buf.grace_started_at_wall = None;
                return;
            }
            Some(i) => i,
        };
        let front_arrived = sorted[idx].arrived_at_wall;
        if now_wall.duration_since(front_arrived) < grace {
            // Reseat grace clock to *this* terminal's arrival — earlier
            // terminals may already have been consumed in prior iterations.
            buf.grace_started_at_wall = Some(front_arrived);
            return;
        }

        let terminal_ts = sorted[idx].ic.call.request_time;
        // Partition: every pending key ≤ terminal_ts → this turn.
        let turn_keys: Vec<i64> = buf.pending.range(..=terminal_ts).map(|(k, _)| *k).collect();
        let mut turn_calls: Vec<BufferedCall> = Vec::new();
        for k in turn_keys {
            if let Some(v) = buf.pending.remove(&k) {
                turn_calls.extend(v);
            }
        }

        // `emit_or_discard` returns whether this partition actually produced
        // a Completed event; the discard rule can drop partitions with no
        // user_turn_start. We must NOT inspect `events.last()` here because
        // the loop may have already pushed a Completed in a previous
        // iteration, which would falsely bump the grace counter for a
        // discarded partition.
        let emitted = emit_or_discard(
            profile,
            wire_apis,
            source_id,
            session_id,
            &turn_calls,
            metrics,
            events,
            FinalizeKind::Flush, // grace-driven; only bump grace counter if actually emitted
        );
        if emitted {
            metrics.counter(Metric::TurnClosedByGrace).inc();
        }

        buf.last_finalized_request_time = Some(terminal_ts);
        // Loop: the next iteration recomputes the next pending terminal.
    }
}

/// Apply the discard rule and emit (or count-and-drop) one turn per drained
/// partition. The caller has already removed `calls` from the buffer and
/// updated bookkeeping.
///
/// Returns `true` iff a `TurnEvent::Completed` was actually pushed — callers
/// that need to count grace-finalizations can rely on this instead of
/// peeking at `events.last()`, which is unsafe when the loop has produced
/// earlier Completed events in prior iterations.
#[allow(clippy::too_many_arguments)]
#[must_use]
fn emit_or_discard(
    profile: &dyn AgentProfile,
    wire_apis: &WireApiRegistry,
    source_id: &str,
    session_id: &str,
    calls: &[BufferedCall],
    metrics: &MetricsWorker,
    events: &mut Vec<TurnEvent>,
    kind: FinalizeKind,
) -> bool {
    if calls.is_empty() {
        return false;
    }
    let has_user_start = calls
        .iter()
        .any(|bc| profile.is_user_turn_start(&bc.ic.call) == Some(true));
    if !has_user_start {
        metrics.counter(Metric::TurnDiscardedNoUserStart).inc();
        return false;
    }

    let refs: Vec<&AgentCall> = calls.iter().map(|bc| &bc.ic).collect();
    let mut turn = build_turn(profile, wire_apis, &refs);
    // The buffer key is authoritative for source/session — call.source_id
    // can legitimately be empty in tests; identity.session_id may differ
    // from a future cross-bucket scheme. Using the key keeps us consistent.
    turn.source_id = source_id.to_string();
    turn.session_id = session_id.to_string();
    events.push(TurnEvent::Completed(turn));
    metrics.counter(Metric::TurnsCompleted).inc();
    if matches!(kind, FinalizeKind::Idle) {
        metrics.counter(Metric::TurnClosedByIdle).inc();
    }
    true
}

/// "Does this call end the main agent's turn?"
///
/// Sub-agent calls are excluded outright. For profiles that provide an
/// explicit `turn_id_hint` (Codex), only the profile's own `is_turn_terminal`
/// predicate counts — Codex's `finish_reason=completed` means "API call
/// succeeded," not "turn done." For implicit-path profiles (Anthropic), the
/// call's wire_api decides: a finish_reason is terminal iff the WireApi says
/// it's terminal AND not a tool_use. `tool_use` keeps the agent loop running,
/// so it must NOT close the turn.
fn is_main_terminal(
    profile: &dyn AgentProfile,
    wire_apis: &WireApiRegistry,
    ic: &AgentCall,
) -> bool {
    if profile.subagent(&ic.call).is_some() {
        return false;
    }
    if profile.is_turn_terminal(&ic.call) {
        return true;
    }
    if ic.agent.turn_id_hint.is_some() {
        return false;
    }
    let Some(reason) = ic.call.finish_reason.as_deref() else {
        return false;
    };
    let Some(api) = wire_apis.find_by_name(ic.call.wire_api) else {
        return false;
    };
    api.is_terminal(reason) && !api.is_tool_use(reason)
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.iter().any(|v| v == &value) {
        list.push(value);
    }
}

/// Pure constructor over a request_time-sorted, complete partition.
///
/// Turn status and `final_*` are both derived from the real terminal pick
/// (`is_main_terminal`). Wire-level `finish_reason` alone can't distinguish
/// "final answer" from "tool roundtrip pending" on profiles with
/// `turn_id_hint` (codex-cli over openai-responses — every call reports
/// `Complete` regardless of whether the model asked for a tool call). For
/// partitions drained via idle-sweep / EOF-flush with no terminal in the
/// buffer, `final_*` stay `None` and status falls to `Incomplete`.
fn build_turn(
    profile: &dyn AgentProfile,
    wire_apis: &WireApiRegistry,
    calls: &[&AgentCall],
) -> AgentTurn {
    assert!(!calls.is_empty(), "build_turn requires at least one call");
    let first = calls[0];
    let source_id = first.call.source_id.clone();
    let session_id = first.agent.session_id.clone();
    let agent_kind = first.agent.agent_kind.to_string();
    let wire_api = first.call.wire_api.to_string();
    let turn_id = Uuid::now_v7().to_string();

    let start_time_us = first.call.request_time;
    let end_time_us = calls
        .iter()
        .map(|ic| {
            ic.call
                .complete_time
                .or(ic.call.response_time)
                .unwrap_or(ic.call.request_time)
        })
        .max()
        .expect("non-empty");
    let duration_ms = ((end_time_us - start_time_us).max(0) / 1000) as u64;

    let call_count = calls.len() as u32;
    let call_ids: Vec<String> = calls.iter().map(|ic| ic.call.id.clone()).collect();

    let mut models_used: Vec<String> = Vec::new();
    let mut subagents_used: Vec<String> = Vec::new();
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut total_cache_read_input_tokens: u64 = 0;
    let mut total_cache_creation_input_tokens: u64 = 0;
    for ic in calls {
        push_unique(&mut models_used, ic.call.model.clone());
        if let Some(sa) = profile.subagent(&ic.call) {
            push_unique(&mut subagents_used, sa);
        }
        if let Some(t) = ic.call.input_tokens {
            total_input_tokens += t as u64;
        }
        if let Some(t) = ic.call.output_tokens {
            total_output_tokens += t as u64;
        }
        if let Some(t) = ic.call.cache_read_input_tokens {
            total_cache_read_input_tokens += t as u64;
        }
        if let Some(t) = ic.call.cache_creation_input_tokens {
            total_cache_creation_input_tokens += t as u64;
        }
    }

    // user_input: prefer the main-agent call flagged as user-turn-start.
    // Fall back to any main-agent call whose body yields user input.
    let user_pick = calls
        .iter()
        .find(|ic| {
            profile.subagent(&ic.call).is_none()
                && profile.is_user_turn_start(&ic.call) == Some(true)
        })
        .or_else(|| {
            calls.iter().find(|ic| {
                profile.subagent(&ic.call).is_none()
                    && profile.extract_user_input(&ic.call).is_some()
            })
        });
    let (user_input_preview, user_call_id) = match user_pick {
        Some(ic) => match profile.extract_user_input(&ic.call) {
            Some(text) => (
                Some(truncate_preview(&text, USER_INPUT_PREVIEW_CHARS)),
                Some(ic.call.id.clone()),
            ),
            None => (None, None),
        },
        None => (None, None),
    };

    // Terminal pick: the main-agent call that actually closed this turn.
    // Drives both `status` and `final_*`. `None` → turn is Incomplete (buffer
    // was drained via sweep/flush before a terminal landed).
    let terminal: Option<&AgentCall> = calls
        .iter()
        .rev()
        .find(|ic| is_main_terminal(profile, wire_apis, ic))
        .copied();

    // `terminal` is `Some` iff a real wire-level terminal landed before
    // finalize. Wire-level vocabulary (`end_turn`, `max_tokens`, `refusal`,
    // `completed`, …) lives entirely in `final_finish_reason` below; status
    // is the binary "did we close cleanly" signal.
    let status = match terminal {
        Some(_) => TurnStatus::Complete,
        None => TurnStatus::Incomplete,
    };

    let (final_answer_preview, final_call_id, final_finish_reason) = match terminal {
        Some(ic) => {
            let preview = profile
                .extract_assistant_text(&ic.call)
                .map(|t| truncate_preview(&t, FINAL_ANSWER_PREVIEW_CHARS));
            (
                preview,
                Some(ic.call.id.clone()),
                ic.call.finish_reason.clone(),
            )
        }
        None => (None, None, None),
    };

    AgentTurn {
        source_id,
        turn_id,
        session_id,
        wire_api,
        agent_kind,
        start_time_us,
        end_time_us,
        duration_ms,
        call_count,
        models_used,
        subagents_used,
        total_input_tokens,
        total_output_tokens,
        total_cache_read_input_tokens,
        total_cache_creation_input_tokens,
        total_cost_usd: None,
        status,
        final_finish_reason,
        user_input_preview,
        user_call_id,
        final_answer_preview,
        final_call_id,
        call_ids,
        metadata: serde_json::json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use ts_common::internal_metrics::MetricsSystem;
    use ts_llm::agents;
    use ts_llm::model::{AgentCall, AgentIdentity, ApiType, LlmCall};
    use ts_llm::wire_apis as wa;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
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
        let _svc = sys.start();
        w
    }

    /// Build a tracker with zero grace so calls finalize on arrival — keeps
    /// unit assertions tight.
    fn mk_tracker_no_grace() -> TurnTracker {
        TurnTracker::new(
            Arc::new(agents::build_default_registry()),
            Arc::new(ts_llm::wire_apis::build_default_wire_api_registry()),
            TrackerConfig {
                grace: Duration::ZERO,
                ..TrackerConfig::default()
            },
            test_metrics(),
        )
    }

    fn ic(call: LlmCall, agent: AgentIdentity) -> AgentCall {
        AgentCall {
            call: Arc::new(call),
            agent,
        }
    }

    fn identity_for_anthropic(call: &LlmCall) -> AgentIdentity {
        let reg = agents::build_default_registry();
        let profile = reg.find_by_name("claude-cli").expect("claude-cli profile");
        let ids = profile.extract_ids(call).expect("ids");
        AgentIdentity {
            agent_kind: "claude-cli",
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        }
    }

    fn identity_for_codex(call: &LlmCall) -> AgentIdentity {
        let reg = agents::build_default_registry();
        let profile = reg.find_by_name("codex-cli").expect("codex-cli profile");
        let ids = profile.extract_ids(call).expect("ids");
        AgentIdentity {
            agent_kind: "codex-cli",
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        }
    }

    fn anthropic_call(
        session: &str,
        request_time_us: i64,
        body_kind: &str,
        finish: &str,
    ) -> LlmCall {
        // Tools: include "Agent" so the call is classified as main-agent
        // (looks_like_subagent → false). Tests that need sub-agent context
        // build their own bodies inline.
        let body = match body_kind {
            "text" => r#"{"messages":[{"role":"user","content":[{"type":"text","text":"go"}]}],"tools":[{"name":"Agent"},{"name":"Bash"}]}"#,
            "tool_result" => r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}],"tools":[{"name":"Agent"},{"name":"Bash"}]}"#,
            _ => unreachable!(),
        }.to_string();
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
            response_body: None,
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
        }
    }

    fn codex_call(
        session: &str,
        turn: &str,
        body_input_type: &str,
        finish: &str,
    ) -> LlmCall {
        let meta = format!(r#"{{"session_id":"{session}","turn_id":"{turn}"}}"#);
        let body = match body_input_type {
            "message" => {
                r#"{"input":[{"type":"message","role":"user","content":"hi"}]}"#.to_string()
            }
            other => format!(r#"{{"input":[{{"type":"{other}"}}]}}"#),
        };
        LlmCall {
            source_id: String::new(),
            id: format!("c-{turn}"),
            wire_api: wa::OPENAI_RESPONSES,
            model: "gpt-5.4".into(),
            api_type: ApiType::Chat,
            request_time: 1_000_000,
            response_time: Some(1_500_000),
            complete_time: Some(2_000_000),
            request_path: "/v1/responses".into(),
            is_stream: true,
            request_body: Some(body),
            status_code: Some(200),
            finish_reason: Some(finish.to_string()),
            response_body: None,
            input_tokens: Some(100),
            output_tokens: Some(10),
            total_tokens: Some(110),
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
                ("Originator".into(), "codex_cli_rs".into()),
                ("X-Codex-Turn-Metadata".into(), meta),
            ],
            response_headers: vec![],
        }
    }

    fn drain_completed(events: Vec<TurnEvent>) -> Vec<AgentTurn> {
        events
            .into_iter()
            .map(|TurnEvent::Completed(t)| t)
            .collect()
    }

    #[test]
    fn tracker_starts_empty() {
        let t = TurnTracker::new(
            Arc::new(AgentProfileRegistry::new()),
            Arc::new(ts_llm::wire_apis::build_default_wire_api_registry()),
            TrackerConfig::default(),
            test_metrics(),
        );
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_on_empty_tracker_returns_no_events() {
        let mut t = TurnTracker::new(
            Arc::new(AgentProfileRegistry::new()),
            Arc::new(ts_llm::wire_apis::build_default_wire_api_registry()),
            TrackerConfig::default(),
            test_metrics(),
        );
        assert!(t.flush_all().is_empty());
    }

    #[test]
    fn anthropic_two_call_turn_finalizes_after_grace() {
        // c1 (user-start, ToolUse) → c2 (cont, Complete = main terminal).
        // grace=0 → c2 grace expires immediately on its own ingest.
        let mut t = mk_tracker_no_grace();
        let c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let c2 = anthropic_call("S", 2_000_000, "tool_result", "end_turn");
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let mut events = t.ingest(ic(c1.clone(), id1));
        events.extend(t.ingest(ic(c2.clone(), id2)));
        let turns = drain_completed(events);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Complete);
        assert_eq!(turns[0].call_count, 2);
        assert_eq!(turns[0].call_ids, vec![c1.id, c2.id]);
    }

    #[test]
    fn anthropic_captures_user_input_and_final_answer() {
        let mut t = mk_tracker_no_grace();
        let mut c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        c1.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>ignore</system-reminder>plan the refactor"}]}],"tools":[{"name":"Agent"},{"name":"Bash"}]}"#.into(),
        );
        let mut c2 = anthropic_call("S", 2_000_000, "tool_result", "end_turn");
        c2.response_body =
            Some(r#"{"content":[{"type":"text","text":"Done. Here is the result."}]}"#.into());
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let c2_id = c2.id.clone();
        let mut events = t.ingest(ic(c1.clone(), id1));
        events.extend(t.ingest(ic(c2, id2)));
        let turns = drain_completed(events);
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(
            turn.user_input_preview.as_deref(),
            Some("plan the refactor")
        );
        assert_eq!(turn.user_call_id.as_deref(), Some(c1.id.as_str()));
        assert_eq!(
            turn.final_answer_preview.as_deref(),
            Some("Done. Here is the result.")
        );
        assert_eq!(turn.final_call_id.as_deref(), Some(c2_id.as_str()));
    }

    #[test]
    fn final_answer_preview_is_truncated() {
        let mut t = mk_tracker_no_grace();
        let long_text = "x".repeat(1000);
        let body = format!(r#"{{"content":[{{"type":"text","text":"{long_text}"}}]}}"#);
        let c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let mut c2 = anthropic_call("S", 2_000_000, "tool_result", "end_turn");
        c2.response_body = Some(body);
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        let turns = drain_completed(events);
        let preview = turns[0].final_answer_preview.as_deref().unwrap();
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), FINAL_ANSWER_PREVIEW_CHARS + 1);
    }

    #[test]
    fn user_input_preview_is_truncated() {
        let mut t = mk_tracker_no_grace();
        let long_text = "u".repeat(1000);
        let body = format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"{long_text}"}}]}}],"tools":[{{"name":"Agent"}},{{"name":"Bash"}}]}}"#
        );
        let mut c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        c1.request_body = Some(body);
        let c2 = anthropic_call("S", 2_000_000, "tool_result", "end_turn");
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let c1_id = c1.id.clone();
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        let turns = drain_completed(events);
        let preview = turns[0].user_input_preview.as_deref().unwrap();
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), USER_INPUT_PREVIEW_CHARS + 1);
        assert_eq!(turns[0].user_call_id.as_deref(), Some(c1_id.as_str()));
    }

    #[test]
    fn subagent_complete_does_not_close_parent_turn() {
        // Sub-agent's Complete is not is_main_terminal, so no grace fires
        // until the main-agent's own terminal arrives.
        let mut t = mk_tracker_no_grace();
        let c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let mut c2 = anthropic_call("S", 2_000_000, "text", "end_turn");
        c2.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],"tools":[{"name":"Read"},{"name":"Grep"}]}"#.into(),
        );
        let c3 = anthropic_call("S", 3_000_000, "tool_result", "tool_use");
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let id3 = identity_for_anthropic(&c3);
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        events.extend(t.ingest(ic(c3, id3)));
        assert!(
            drain_completed(events).is_empty(),
            "no main-agent terminal yet"
        );
        assert_eq!(t.active_count(), 3);
    }

    #[test]
    fn subagent_assistant_text_does_not_leak_to_parent_final_answer() {
        let mut t = mk_tracker_no_grace();
        let mut c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        c1.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"start"}]}],"tools":[{"name":"Agent"}]}"#.into(),
        );
        c1.response_body = Some(r#"{"content":[{"type":"text","text":"parent progress"}]}"#.into());
        let mut c2 = anthropic_call("S", 2_000_000, "text", "end_turn");
        c2.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"sub task"}]}],"tools":[{"name":"Read"}]}"#.into(),
        );
        c2.response_body =
            Some(r#"{"content":[{"type":"text","text":"sub-agent conclusion"}]}"#.into());
        let mut c3 = anthropic_call("S", 3_000_000, "tool_result", "end_turn");
        c3.response_body = Some(r#"{"content":[{"type":"text","text":"final answer"}]}"#.into());
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let id3 = identity_for_anthropic(&c3);
        let c3_id = c3.id.clone();
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        events.extend(t.ingest(ic(c3, id3)));
        let turns = drain_completed(events);
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(turn.final_answer_preview.as_deref(), Some("final answer"));
        assert_eq!(turn.final_call_id.as_deref(), Some(c3_id.as_str()));
        assert!(turn.subagents_used.iter().any(|s| s == "task"));
    }

    #[test]
    fn anthropic_tool_use_keeps_turn_open() {
        // No terminal — buffer grows, no events emitted.
        let mut t = mk_tracker_no_grace();
        let c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let c2 = anthropic_call("S", 2_000_000, "tool_result", "tool_use");
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        assert!(drain_completed(events).is_empty());
        assert_eq!(t.active_count(), 2);
    }

    #[test]
    fn single_call_complete_closes_after_grace() {
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", "end_turn");
        let id = identity_for_anthropic(&c);
        let events = t.ingest(ic(c, id));
        let turns = drain_completed(events);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Complete);
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn single_call_max_tokens_closes_after_grace() {
        // `max_tokens` is wire-terminal (model finished, just truncated). The
        // raw reason lives in `final_finish_reason`; status is the binary
        // "did a terminal land" — Complete.
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", "max_tokens");
        let id = identity_for_anthropic(&c);
        let turns = drain_completed(t.ingest(ic(c, id)));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Complete);
        assert_eq!(
            turns[0].final_finish_reason.as_deref(),
            Some("max_tokens"),
            "raw wire reason preserved on the row"
        );
    }

    #[test]
    fn single_call_pause_turn_stays_buffered() {
        // Anthropic `pause_turn` is intentionally NOT terminal — the
        // server-tool loop yielded mid-turn and the assistant turn continues
        // on a follow-up call. Tracker must keep the call buffered.
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", "pause_turn");
        let id = identity_for_anthropic(&c);
        let events = t.ingest(ic(c, id));
        assert!(
            drain_completed(events).is_empty(),
            "pause_turn is not terminal"
        );
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn auxiliary_call_is_skipped_entirely() {
        let mut t = mk_tracker_no_grace();
        let mut c = anthropic_call("S", 1_000_000, "text", "end_turn");
        c.request_body =
            Some(r#"{"messages":[{"role":"user","content":"generate title"}],"tools":[]}"#.into());
        let id = AgentIdentity {
            agent_kind: "claude-cli",
            session_id: "S".into(),
            turn_id_hint: None,
        };
        let events = t.ingest(ic(c, id));
        assert!(drain_completed(events).is_empty());
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_emits_remaining_buffered_turn_as_incomplete() {
        // Pure user-start, no terminal → flush_all emits Incomplete.
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", "tool_use");
        let id = identity_for_anthropic(&c);
        t.ingest(ic(c, id));
        assert_eq!(t.active_count(), 1);
        let turns = drain_completed(t.flush_all());
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Incomplete);
        // No terminal ⇒ no final_*. Guards against regressing to "last call by
        // request_time wins" which would mislabel the ToolUse call as final.
        assert!(turns[0].final_call_id.is_none());
        assert!(turns[0].final_finish_reason.is_none());
        assert!(turns[0].final_answer_preview.is_none());
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_discards_partition_without_user_start() {
        // A lone continuation: terminal but no user-start → discard, not emit.
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "tool_result", "end_turn");
        let id = identity_for_anthropic(&c);
        // is_user_turn_start returns Some(false) for tool_result-only body.
        let turns = drain_completed(t.ingest(ic(c, id)));
        assert!(turns.is_empty(), "no user_start partition is dropped");
    }

    #[test]
    fn codex_complete_does_not_close_turn_immediately() {
        // turn_id_hint=Some + finish=Complete + no terminal-output predicate
        // → not main-terminal. Buffer grows; no events.
        let mut t = mk_tracker_no_grace();
        let c = codex_call("s1", "t1", "message", "completed");
        let id = identity_for_codex(&c);
        let events = t.ingest(ic(c, id));
        assert!(drain_completed(events).is_empty());
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn codex_terminal_output_closes_after_grace() {
        let mut t = mk_tracker_no_grace();
        let mut c = codex_call("s1", "t1", "message", "completed");
        c.response_body = Some(
            r#"{"output":[
                {"type":"reasoning","summary":[]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"done."}]}
            ]}"#
            .to_string(),
        );
        let id = identity_for_codex(&c);
        let turns = drain_completed(t.ingest(ic(c, id)));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Complete);
    }

    #[test]
    fn codex_pending_function_call_keeps_turn_open() {
        let mut t = mk_tracker_no_grace();
        let mut c = codex_call("s1", "t1", "message", "completed");
        c.response_body = Some(
            r#"{"output":[
                {"type":"reasoning","summary":[]},
                {"type":"function_call","name":"shell","call_id":"c1","arguments":"{}"}
            ]}"#
            .to_string(),
        );
        let id = identity_for_codex(&c);
        let events = t.ingest(ic(c, id));
        assert!(drain_completed(events).is_empty());
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn codex_flush_with_pending_function_call_tail_is_incomplete() {
        // Regression: a codex turn whose tail is a function_call (e.g.
        // exec_command) with no function_call_output roundtrip in the buffer
        // must not be mislabelled Complete. Per-call finish_reason is always
        // `Complete` on openai-responses; only `is_turn_terminal` (no *_call
        // in output) can tell "final answer" apart from "tool pending".
        let mut t = mk_tracker_no_grace();
        let mut c = codex_call("s1", "t1", "message", "completed");
        c.response_body = Some(
            r#"{"output":[
                {"type":"reasoning","summary":[]},
                {"type":"function_call","name":"exec_command","call_id":"ec1","arguments":"{}"}
            ]}"#
            .to_string(),
        );
        let id = identity_for_codex(&c);
        t.ingest(ic(c, id));
        assert_eq!(t.active_count(), 1);
        let turns = drain_completed(t.flush_all());
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Incomplete);
        assert!(turns[0].final_call_id.is_none());
        assert!(turns[0].final_finish_reason.is_none());
        assert!(turns[0].final_answer_preview.is_none());
    }

    #[test]
    fn sweep_idle_buffer_emits_incomplete_with_user_start() {
        let cfg = TrackerConfig {
            grace: Duration::ZERO,
            idle_timeout_us: 1_000,
            sweep_interval_us: 1_000,
        };
        let mut t = TurnTracker::new(
            Arc::new(agents::build_default_registry()),
            Arc::new(ts_llm::wire_apis::build_default_wire_api_registry()),
            cfg,
            test_metrics(),
        );
        // Non-terminal call (ToolUse) — sits in buffer with no grace.
        let c = anthropic_call("S", 1_000_000, "text", "tool_use");
        let source_id = c.source_id.clone();
        let id = identity_for_anthropic(&c);
        let arrival = c.complete_time.unwrap();
        t.ingest(ic(c, id));
        assert_eq!(t.active_count(), 1);

        // Idle sweep is event-time driven; wall-clock arg doesn't matter here.
        let swept =
            drain_completed(t.advance_time_at(arrival + 1_000_000, &source_id, Instant::now()));
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].status, TurnStatus::Incomplete);
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn ingest_populates_call_ids_into_finalized_turn() {
        let mut t = mk_tracker_no_grace();
        // Two codex calls, second is terminal-output → grace fires.
        let mut c1 = codex_call("s1", "t1", "message", "completed");
        c1.id = "c1".into();
        c1.request_time = 1_000_000;
        c1.complete_time = Some(1_500_000);
        let mut c2 = codex_call("s1", "t1", "message", "completed");
        c2.id = "c2".into();
        c2.request_time = 2_000_000;
        c2.complete_time = Some(2_500_000);
        c2.response_body = Some(
            r#"{"output":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"done."}]}
            ]}"#
            .to_string(),
        );
        let id1 = identity_for_codex(&c1);
        let id2 = identity_for_codex(&c2);
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        let turns = drain_completed(events);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].call_ids, vec!["c1".to_string(), "c2".to_string()]);
    }

    #[test]
    fn orphan_call_strictly_older_than_finalized_is_dropped() {
        let mut t = mk_tracker_no_grace();
        let c1 = anthropic_call("S", 2_000_000, "text", "tool_use");
        let c2 = anthropic_call("S", 3_000_000, "tool_result", "end_turn");
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        t.ingest(ic(c1, id1));
        let _ = t.ingest(ic(c2, id2));
        // Now last_finalized_request_time = 3_000_000 for this session.
        let c0 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let id0 = identity_for_anthropic(&c0);
        let events = t.ingest(ic(c0, id0));
        assert!(
            drain_completed(events).is_empty(),
            "orphan must not produce a turn"
        );
        // And nothing should now be buffered for this session beyond what's
        // already finalized — c0 was dropped at the entry guard.
        assert_eq!(t.active_count(), 0);
    }
}
