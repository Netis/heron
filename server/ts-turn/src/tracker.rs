//! Buffer-and-finalize turn tracker. See `docs/design/04-turn.md`.
//!
//! Each `(source_id, session_id)` owns a `SessionBuffer` that holds calls
//! sorted by `request_time` until a main-agent terminal call appears and its
//! grace window elapses. On grace expiry the buffer is partitioned at each
//! terminal and every partition becomes one `AgentTurn`. Partitions that
//! contain no `is_user_turn_start = Some(true)` call are discarded.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use uuid::Uuid;

use ts_common::agent::{AgentTopology, ToolSurface};
use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_llm::agent_classifier::SuspiciousSignal;
use ts_llm::model::AgentCall;

use crate::model::{ActiveTurnRegistry, AgentTurn, TurnStatus};
use crate::SuspiciousSkillRollup;

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
    /// Cached "this call is a main-agent turn terminator" — i.e.
    /// `agent.subagent_name.is_none() && agent.is_turn_terminal`. Cached at
    /// ingest time so finalize doesn't re-compose per element.
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
    /// Stable `turn_id` for the in-progress turn currently assembling in
    /// this buffer. Minted lazily on the first ingest after the previous
    /// turn was finalized; reused by every snapshot inserted into the
    /// `ActiveTurnRegistry` and by the eventual `Completed` turn so the
    /// in-memory in-progress entry's id matches the row that gets
    /// persisted (lets the API drop the registry entry without a
    /// frontend "row jump"). Cleared on every finalize / sweep / flush.
    current_turn_id: Option<String>,
}

/// Single stateful owner of turn assembly. Passive: callers drive it via
/// `ingest`, `advance_time`, `sweep`, `flush_all`.
///
/// Pure assembly stage: classification (`is_turn_terminal`, `is_user_turn_start`,
/// `subagent_name`, `is_auxiliary`, `user_input`, `assistant_text`) is decided
/// once at the ts-llm boundary and carried on `AgentCall.agent`. The tracker
/// only reads those fields — no profile or wire-api registry held here.
/// "Main-agent terminal" is composed at read sites as
/// `agent.subagent_name.is_none() && agent.is_turn_terminal`.
///
/// Two clocks by design:
/// * `virtual_now_by_source` — per-source event-time watermark. Bumped by
///   each source's own heartbeats and ingested calls. Used for idle sweep,
///   gc, and the orphan guard. Per-source so one source's activity cannot
///   fast-forward another source's idle/gc horizon.
/// * `Instant` taken at ingest / advance — wall-clock. Used only for grace.
pub struct TurnTracker {
    config: TrackerConfig,
    buffers: HashMap<(String, String), SessionBuffer>,
    /// Event-time watermark, keyed by `source_id`. `0` ≡ "never seen".
    virtual_now_by_source: HashMap<String, i64>,
    /// Global sweep-interval throttle, in event-time µs. Not per-source — it
    /// only controls how often `sweep` iterates buffers, not correctness.
    last_sweep_us: i64,
    metrics: MetricsWorker,
    /// Optional in-memory registry of in-progress turns, shared between
    /// every tracker and the API handler. When present, every `ingest_at`
    /// upserts a snapshot into the registry, and every finalize /
    /// sweep / flush removes the entry. `None` is used by tests that
    /// don't care about the snapshot path.
    active_registry: Option<ActiveTurnRegistry>,
}

impl TurnTracker {
    pub fn new(config: TrackerConfig, metrics: MetricsWorker) -> Self {
        Self::with_registry(config, metrics, None)
    }

    /// Variant that wires an [`ActiveTurnRegistry`] into the tracker so
    /// the API can read in-progress snapshots without going through the
    /// DB or a channel fan-out. Pipelines call this; tests that don't
    /// exercise the snapshot path use [`Self::new`].
    pub fn with_registry(
        config: TrackerConfig,
        metrics: MetricsWorker,
        active_registry: Option<ActiveTurnRegistry>,
    ) -> Self {
        Self {
            config,
            buffers: HashMap::new(),
            virtual_now_by_source: HashMap::new(),
            last_sweep_us: 0,
            metrics,
            active_registry,
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
        let session_id = ic.agent.session_id.clone();
        let virtual_now = self.bump_event_time(&source_id, arrival_ts);
        self.metrics.counter(Metric::TurnCallsIngested).inc();

        // Auxiliary one-shots (e.g., claude-cli session-title) bypass turn
        // assembly entirely. They still flow to storage independently.
        if ic.agent.is_auxiliary {
            self.metrics.counter(Metric::TurnCallsAuxiliary).inc();
            return self.flush_ready_buffers(now_wall);
        }

        let key = (source_id.clone(), session_id.clone());
        let buf = self.buffers.entry(key).or_default();

        if let Some(hw) = buf.last_finalized_request_time {
            if ic.call.request_time < hw {
                self.metrics.counter(Metric::TurnCallsDroppedLate).inc();
                return self.flush_ready_buffers(now_wall);
            }
        }

        // Main-agent terminal = sub-agent layering filter ANDed with the raw
        // protocol-terminal verdict from ts-llm. Same composition pattern as
        // the user-start check in `emit_or_discard`.
        let is_terminal = ic.agent.subagent_name.is_none() && ic.agent.is_turn_terminal;
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

        // Mint a stable `turn_id` for this in-progress turn the first time we
        // see a call after the previous turn finalized. Reused by every
        // registry snapshot AND by the eventual persisted `Completed`, so the
        // `agent_turns` row's `turn_id` matches the in-progress entry the
        // console has been showing. Cleared by `emit_or_discard` after
        // finalize so the next ingest mints a fresh one.
        if buf.current_turn_id.is_none() {
            buf.current_turn_id = Some(Uuid::now_v7().to_string());
        }

        // Refresh the registry's snapshot for this turn. Cheap (one HashMap
        // upsert + an AgentTurn alloc); skipped if no registry is wired (e.g.
        // tests via `TurnTracker::new`).
        self.refresh_active_snapshot(&source_id, &session_id);

        self.flush_ready_buffers(now_wall)
    }

    /// Build the in-progress AgentTurn snapshot for a given session and
    /// upsert it into the active registry. Truncates the partition at the
    /// first main-agent terminal so multi-turn buffers don't mix two
    /// turns' state into one snapshot. Applies the same discard rule as
    /// `emit_or_discard` so we don't leak phantom in-progress rows for
    /// sub-agent-only filler sessions.
    fn refresh_active_snapshot(&self, source_id: &str, session_id: &str) {
        let registry = match &self.active_registry {
            Some(r) => r,
            None => return,
        };
        let buf = match self
            .buffers
            .get(&(source_id.to_string(), session_id.to_string()))
        {
            Some(b) => b,
            None => return,
        };
        let turn_id = match &buf.current_turn_id {
            Some(id) => id.clone(),
            None => return,
        };

        // Partition: calls up to and including the first main-agent terminal.
        let partition: Vec<&AgentCall> = {
            let mut sorted: Vec<&BufferedCall> = buf.pending.values().flatten().collect();
            sorted.sort_by_key(|bc| bc.ic.call.request_time);
            let mut acc: Vec<&AgentCall> = Vec::with_capacity(sorted.len());
            for bc in sorted {
                acc.push(&bc.ic);
                if bc.is_terminal {
                    break;
                }
            }
            acc
        };
        if partition.is_empty() {
            return;
        }
        // Same discard rule as emit_or_discard — only show in-progress rows
        // whose partition has a main-agent user_turn_start. Without this, a
        // sub-agent dispatch's first call (which carries
        // `is_user_turn_start=Some(true)` AND `subagent_name=Some(_)`) would
        // produce a phantom row that finalize will eventually drop.
        let has_user_start = partition.iter().any(|ic| {
            ic.agent.subagent_name.is_none() && ic.agent.is_user_turn_start == Some(true)
        });
        if !has_user_start {
            return;
        }

        let mut snap = build_turn(
            &partition,
            Some(turn_id.clone()),
            Some(TurnStatus::InProgress),
        );
        snap.source_id = source_id.to_string();
        snap.session_id = session_id.to_string();

        if let Ok(mut map) = registry.write() {
            map.insert(turn_id, snap);
        }
    }

    /// Read-side helper used by tests to inspect the registry contents
    /// without going through the lock directly. Returns a clone of every
    /// in-progress snapshot currently registered. Production reads happen
    /// in `ts-api` directly against the `ActiveTurnRegistry` Arc.
    pub fn snapshot_active(&self) -> Vec<AgentTurn> {
        match &self.active_registry {
            Some(reg) => reg
                .read()
                .map(|map| map.values().cloned().collect())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Walk every buffer; for each whose front-pending terminal's grace has
    /// expired (by wall-clock), hand off to `finalize_session` to emit one or
    /// more turns.
    fn flush_ready_buffers(&mut self, now_wall: Instant) -> Vec<TurnEvent> {
        let grace = self.config.grace;

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
            let buf = self.buffers.get_mut(&key).expect("key just listed");
            finalize_session(
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
        self.drop_completed_from_registry(&events);
        events
    }

    /// After any Completed turn is emitted, drop its in-progress entry
    /// from the registry — the persisted DB row replaces it in the API's
    /// UNION view, so leaving the snapshot would double-count. Idempotent;
    /// safe to call with stacked-partition turn_ids that were never in
    /// the registry.
    fn drop_completed_from_registry(&self, events: &[TurnEvent]) {
        let registry = match &self.active_registry {
            Some(r) => r,
            None => return,
        };
        let mut map = match registry.write() {
            Ok(g) => g,
            Err(_) => return,
        };
        for ev in events {
            let TurnEvent::Completed(t) = ev;
            map.remove(&t.turn_id);
        }
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
        self.metrics.counter(Metric::TurnHeartbeatsReceived).inc();
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
            // Reuse the in-progress turn_id so the persisted Incomplete
            // turn shares the same id the registry has been advertising.
            // The registry entry will be dropped at the end of this fn.
            let turn_id_override = buf.current_turn_id.take();

            // sweep already counts itself via FinalizeKind::Idle inside
            // emit_or_discard; the returned bool isn't needed here.
            let _ = emit_or_discard(
                &key.0,
                &key.1,
                &drained,
                turn_id_override,
                &self.metrics,
                &mut events,
                FinalizeKind::Idle,
            );
        }
        self.gc_buffers();
        self.drop_completed_from_registry(&events);
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
            // Step 1: drive the terminal-bounded partitions to finalize.
            {
                let buf = self.buffers.get_mut(&key).expect("listed above");
                finalize_session(
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
            // Step 1's finalize_session may already have consumed
            // current_turn_id; if any non-terminal tail is still here it
            // belongs to a turn that the shutdown caught mid-flight. Reuse
            // the id when present so the API's UNION view doesn't briefly
            // show two rows (in-progress + Incomplete) for the same turn.
            let turn_id_override = buf.current_turn_id.take();
            // flush_all's non-terminal tail doesn't have a dedicated counter
            // beyond TurnsCompleted (bumped inside emit_or_discard).
            let _ = emit_or_discard(
                &key.0,
                &key.1,
                &drained,
                turn_id_override,
                &self.metrics,
                &mut events,
                FinalizeKind::Flush,
            );
        }
        self.drop_completed_from_registry(&events);
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
fn finalize_session(
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
        //
        // First pass through the loop: hand the buffer's in-progress
        // current_turn_id to emit_or_discard so the persisted Completed
        // shares the id with the active-registry snapshot. Subsequent
        // partitions in the same buffer are independent turns that never
        // had an in-progress entry, so they get fresh UUIDs (None →
        // build_turn mints).
        let turn_id_override = buf.current_turn_id.take();
        let emitted = emit_or_discard(
            source_id,
            session_id,
            &turn_calls,
            turn_id_override,
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
/// updated bookkeeping. Pass `turn_id_override` to reuse the in-progress
/// snapshot's `turn_id` (so the persisted `Completed` carries the same id
/// the active registry has been advertising — when the API drops the
/// registry entry on UNION, the row appears to "transition" in place
/// rather than disappear-and-reappear). `None` lets `build_turn` mint a
/// fresh UUID — used by partitions that never had an in-progress entry
/// (stacked terminals in one finalize_session loop, shutdown drains).
///
/// Returns `true` iff a `TurnEvent::Completed` was actually pushed — callers
/// that need to count grace-finalizations can rely on this instead of
/// peeking at `events.last()`, which is unsafe when the loop has produced
/// earlier Completed events in prior iterations.
#[must_use]
fn emit_or_discard(
    source_id: &str,
    session_id: &str,
    calls: &[BufferedCall],
    turn_id_override: Option<String>,
    metrics: &MetricsWorker,
    events: &mut Vec<TurnEvent>,
    kind: FinalizeKind,
) -> bool {
    if calls.is_empty() {
        return false;
    }
    // Discard rule: a partition needs at least one *main-agent* user-turn-start.
    // Sub-agent dispatches whose body looks like a fresh user message (e.g.
    // Codex sub-agent dispatch with `input[-1]=message(role=user)`) carry
    // `is_user_turn_start=Some(true)` *and* `subagent_name=Some(_)`; without
    // the sub-agent guard here those would slip through and produce phantom
    // "Incomplete with user_input=None" turns.
    let has_user_start = calls.iter().any(|bc| {
        bc.ic.agent.subagent_name.is_none() && bc.ic.agent.is_user_turn_start == Some(true)
    });
    if !has_user_start {
        metrics.counter(Metric::TurnDiscardedNoUserStart).inc();
        return false;
    }

    let refs: Vec<&AgentCall> = calls.iter().map(|bc| &bc.ic).collect();
    let mut turn = build_turn(&refs, turn_id_override, None);
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

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.iter().any(|v| v == &value) {
        list.push(value);
    }
}

/// Pure constructor over a request_time-sorted partition.
///
/// `turn_id_override`: caller-supplied id. Used by the in-progress
/// snapshot path so every snapshot for the same in-flight turn shares
/// one id, and the eventual finalized `AgentTurn` carries the same id
/// the registry has been advertising. `None` mints a fresh UUID.
///
/// `status_override`: forces a status. Used by the in-progress snapshot
/// path to pin `InProgress` even when the partition's terminal call has
/// already landed (which can happen briefly during the grace window —
/// the user shouldn't see the row flip via a snapshot; that transition
/// belongs to the persisted Completed). `None` falls back to derived
/// status (Complete iff a main-agent terminal is in the partition;
/// Incomplete otherwise).
///
/// Turn status and `final_*` are both derived from the composed main-agent
/// terminal pick — `agent.subagent_name.is_none() && agent.is_turn_terminal`,
/// where the protocol-terminal field is set at ts-llm time. Profiles whose
/// wire-level `finish_reason` cannot distinguish "final answer" from "tool
/// roundtrip pending" (e.g. codex-cli over openai-responses, where every
/// successful call reports `response.completed`) override
/// `AgentProfile::is_turn_terminal` to inspect the body explicitly; the
/// canonical decision lives upstream. For partitions drained via idle-sweep
/// / EOF-flush with no terminal in the buffer, `final_*` stay `None` and
/// status falls to `Incomplete`.
fn build_turn(
    calls: &[&AgentCall],
    turn_id_override: Option<String>,
    status_override: Option<TurnStatus>,
) -> AgentTurn {
    assert!(!calls.is_empty(), "build_turn requires at least one call");
    let first = calls[0];
    let source_id = first.call.source_id.clone();
    let session_id = first.agent.session_id.clone();
    let agent_kind = first.agent.agent_kind.to_string();
    let wire_api = first.call.wire_api.to_string();
    let client_ip = first.call.client_ip;
    let server_ip = first.call.server_ip;
    let turn_id = turn_id_override.unwrap_or_else(|| Uuid::now_v7().to_string());

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
        if let Some(sa) = ic.agent.subagent_name.as_ref() {
            push_unique(&mut subagents_used, sa.clone());
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
    // Fall back to any main-agent call whose body yielded user_input upstream.
    let user_pick = calls
        .iter()
        .find(|ic| ic.agent.subagent_name.is_none() && ic.agent.is_user_turn_start == Some(true))
        .or_else(|| {
            calls
                .iter()
                .find(|ic| ic.agent.subagent_name.is_none() && ic.agent.user_input.is_some())
        });
    let (user_input_preview, user_call_id) = match user_pick {
        Some(ic) => match ic.agent.user_input.as_deref() {
            Some(text) => (
                Some(truncate_preview(text, USER_INPUT_PREVIEW_CHARS)),
                Some(ic.call.id.clone()),
            ),
            None => (None, None),
        },
        None => (None, None),
    };

    // Terminal pick: the main-agent call that actually closed this turn.
    // Drives both `status` and `final_*`. `None` → turn is Incomplete (buffer
    // was drained via sweep/flush before a terminal landed). Same
    // sub-agent + protocol-terminal composition as in `ingest_at`.
    let terminal: Option<&AgentCall> = calls
        .iter()
        .rev()
        .find(|ic| ic.agent.subagent_name.is_none() && ic.agent.is_turn_terminal)
        .copied();

    // `terminal` is `Some` iff a real wire-level terminal landed before
    // finalize. Wire-level vocabulary (`end_turn`, `max_tokens`, `refusal`,
    // `completed`, …) lives entirely in `final_finish_reason` below; status
    // is the lifecycle signal. `status_override` lets the in-progress
    // snapshot path pin `InProgress` even when the terminal has already
    // landed in the partition (which can happen briefly during the grace
    // window — the row should flip to Complete only via the Completed
    // event, never via an in-progress snapshot).
    let status = status_override.unwrap_or_else(|| match terminal {
        Some(_) => TurnStatus::Complete,
        None => TurnStatus::Incomplete,
    });

    let (final_answer_preview, final_call_id, final_finish_reason) = match terminal {
        Some(ic) => {
            let preview = ic
                .agent
                .assistant_text
                .as_deref()
                .map(|t| truncate_preview(t, FINAL_ANSWER_PREVIEW_CHARS));
            (
                preview,
                Some(ic.call.id.clone()),
                ic.call.finish_reason.clone(),
            )
        }
        None => (None, None, None),
    };

    // Agent-classification rollup across all calls in the partition.
    // Topology precedence: Orchestrator > SubAgent > SingleAgent.
    let mut surfaces: BTreeSet<ToolSurface> = BTreeSet::new();
    let mut tool_call_total: u32 = 0;
    let mut topology_rank: u8 = 0;
    let mut topology: Option<AgentTopology> = None;
    let mut suspicious: Vec<SuspiciousSkillRollup> = Vec::new();
    for ic in calls {
        if let Some(s) = ic.agent.tool_surface {
            surfaces.insert(s);
        }
        tool_call_total = tool_call_total.saturating_add(ic.agent.tool_call_count);

        if let Some(t) = ic.agent.agent_topology {
            let rank = match t {
                AgentTopology::Orchestrator => 2,
                AgentTopology::SubAgent => 1,
                AgentTopology::SingleAgent => 0,
            };
            if rank >= topology_rank {
                topology_rank = rank;
                topology = Some(t);
            }
        }

        for sig in &ic.agent.suspicious_signals {
            match sig {
                SuspiciousSignal::UnknownToolName { name } => {
                    suspicious.push(SuspiciousSkillRollup {
                        tool_name: name.clone(),
                        reason: "unknown_tool_name".to_string(),
                    });
                }
            }
        }
    }
    // Deduplicate suspicious by tool_name (keep first occurrence).
    let mut seen: BTreeSet<String> = BTreeSet::new();
    suspicious.retain(|s| seen.insert(s.tool_name.clone()));
    let tool_surfaces: Vec<ToolSurface> = surfaces.into_iter().collect();

    AgentTurn {
        source_id,
        turn_id,
        session_id,
        wire_api,
        agent_kind,
        client_ip,
        server_ip,
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
        tool_surfaces,
        tool_call_total,
        agent_topology: topology,
        suspicious_skills: suspicious,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_common::internal_metrics::MetricsSystem;
    use ts_llm::agents;
    use ts_llm::model::{AgentCall, AgentCallInfo, ApiType, LlmCall};
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
                Metric::TurnHeartbeatsReceived,
            ],
        );
        let _svc = sys.start();
        w
    }

    fn llm_test_metrics() -> MetricsWorker {
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

    /// Build a tracker with zero grace so calls finalize on arrival — keeps
    /// unit assertions tight.
    fn mk_tracker_no_grace() -> TurnTracker {
        TurnTracker::new(
            TrackerConfig {
                grace: Duration::ZERO,
                ..TrackerConfig::default()
            },
            test_metrics(),
        )
    }

    fn ic(call: LlmCall, agent: AgentCallInfo) -> AgentCall {
        AgentCall {
            call: Arc::new(call),
            agent,
        }
    }

    fn call_info_for_anthropic(call: &LlmCall) -> AgentCallInfo {
        let reg = agents::build_default_registry();
        let wa_reg = ts_llm::wire_apis::build_default_wire_api_registry();
        let metrics = llm_test_metrics();
        ts_llm::build_agent_call_info(
            call,
            &reg,
            &wa_reg,
            &ts_llm::agent_classifier::ClassifierConfig::default(),
            &metrics,
        )
        .expect("claude-cli call info")
    }

    fn call_info_for_codex(call: &LlmCall) -> AgentCallInfo {
        let reg = agents::build_default_registry();
        let wa_reg = ts_llm::wire_apis::build_default_wire_api_registry();
        let metrics = llm_test_metrics();
        ts_llm::build_agent_call_info(
            call,
            &reg,
            &wa_reg,
            &ts_llm::agent_classifier::ClassifierConfig::default(),
            &metrics,
        )
        .expect("codex-cli call info")
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
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
        }
    }

    fn codex_call(session: &str, turn: &str, body_input_type: &str, finish: &str) -> LlmCall {
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
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
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
        let t = TurnTracker::new(TrackerConfig::default(), test_metrics());
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_on_empty_tracker_returns_no_events() {
        let mut t = TurnTracker::new(TrackerConfig::default(), test_metrics());
        assert!(t.flush_all().is_empty());
    }

    #[test]
    fn anthropic_two_call_turn_finalizes_after_grace() {
        // c1 (user-start, ToolUse) → c2 (cont, Complete = main terminal).
        // grace=0 → c2 grace expires immediately on its own ingest.
        let mut t = mk_tracker_no_grace();
        let c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let c2 = anthropic_call("S", 2_000_000, "tool_result", "end_turn");
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
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
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
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
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
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
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
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
        // Sub-agent's Complete fails the main-agent terminal composition
        // (subagent_name=Some), so no grace fires until the main-agent's own
        // terminal arrives.
        let mut t = mk_tracker_no_grace();
        let c1 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let mut c2 = anthropic_call("S", 2_000_000, "text", "end_turn");
        c2.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],"tools":[{"name":"Read"},{"name":"Grep"}]}"#.into(),
        );
        let c3 = anthropic_call("S", 3_000_000, "tool_result", "tool_use");
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
        let id3 = call_info_for_anthropic(&c3);
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
    fn subagent_only_partition_with_user_turn_start_is_discarded() {
        // Regression: a partition containing ONLY sub-agent calls — at least
        // one of which carries `is_user_turn_start = Some(true)` (post-Phase 4
        // claude-cli profile reports the structural verdict; Codex's
        // header-based sub-agent had this all along) — must NOT emit a turn.
        // The discard rule in `emit_or_discard` requires the user-start to
        // come from a *main-agent* call.
        //
        // Without the sub-agent guard in `has_user_start`, this case produced a
        // phantom Incomplete turn with `user_input_preview = None` and
        // `final_call_id = None`.
        let mut t = mk_tracker_no_grace();
        let mut c = anthropic_call("S", 1_000_000, "text", "end_turn");
        // Sub-agent body: fresh user text but `tools` lacks "Agent" → profile
        // tags `subagent_name = Some("task")` and (post-Phase-4) returns
        // `is_user_turn_start = Some(true)`.
        c.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],"tools":[{"name":"Read"},{"name":"Grep"}]}"#.into(),
        );
        let id = call_info_for_anthropic(&c);
        // Sanity: classification fields match the bug-trigger preconditions.
        // `is_turn_terminal=true` is fine — it's the raw protocol verdict.
        // The composition `subagent_name.is_none() && is_turn_terminal` is
        // what tracker uses for the main-agent terminal check.
        assert_eq!(id.subagent_name.as_deref(), Some("task"));
        assert_eq!(id.is_user_turn_start, Some(true));
        assert!(
            !(id.subagent_name.is_none() && id.is_turn_terminal),
            "sub-agent call must not be main-terminal in the composed view"
        );

        let mut events = t.ingest(ic(c, id));
        events.extend(t.flush_all());
        assert!(
            drain_completed(events).is_empty(),
            "sub-agent-only partition must be discarded, never emitted"
        );
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
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
        let id3 = call_info_for_anthropic(&c3);
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
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        assert!(drain_completed(events).is_empty());
        assert_eq!(t.active_count(), 2);
    }

    #[test]
    fn single_call_complete_closes_after_grace() {
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", "end_turn");
        let id = call_info_for_anthropic(&c);
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
        let id = call_info_for_anthropic(&c);
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
        let id = call_info_for_anthropic(&c);
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
        let id = call_info_for_anthropic(&c);
        let events = t.ingest(ic(c, id));
        assert!(drain_completed(events).is_empty());
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_emits_remaining_buffered_turn_as_incomplete() {
        // Pure user-start, no terminal → flush_all emits Incomplete.
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", "tool_use");
        let id = call_info_for_anthropic(&c);
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
        let id = call_info_for_anthropic(&c);
        // is_user_turn_start returns Some(false) for tool_result-only body.
        let turns = drain_completed(t.ingest(ic(c, id)));
        assert!(turns.is_empty(), "no user_start partition is dropped");
    }

    #[test]
    fn codex_complete_does_not_close_turn_immediately() {
        // Codex profile overrides is_turn_terminal to check response.output;
        // a "completed" finish_reason without a terminal-output body keeps
        // is_turn_terminal=false. Buffer grows; no events.
        let mut t = mk_tracker_no_grace();
        let c = codex_call("s1", "t1", "message", "completed");
        let id = call_info_for_codex(&c);
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
        let id = call_info_for_codex(&c);
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
        let id = call_info_for_codex(&c);
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
        let id = call_info_for_codex(&c);
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
        let mut t = TurnTracker::new(cfg, test_metrics());
        // Non-terminal call (ToolUse) — sits in buffer with no grace.
        let c = anthropic_call("S", 1_000_000, "text", "tool_use");
        let source_id = c.source_id.clone();
        let id = call_info_for_anthropic(&c);
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
        let id1 = call_info_for_codex(&c1);
        let id2 = call_info_for_codex(&c2);
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
        let id1 = call_info_for_anthropic(&c1);
        let id2 = call_info_for_anthropic(&c2);
        t.ingest(ic(c1, id1));
        let _ = t.ingest(ic(c2, id2));
        // Now last_finalized_request_time = 3_000_000 for this session.
        let c0 = anthropic_call("S", 1_000_000, "text", "tool_use");
        let id0 = call_info_for_anthropic(&c0);
        let events = t.ingest(ic(c0, id0));
        assert!(
            drain_completed(events).is_empty(),
            "orphan must not produce a turn"
        );
        // And nothing should now be buffered for this session beyond what's
        // already finalized — c0 was dropped at the entry guard.
        assert_eq!(t.active_count(), 0);
    }

    // -----------------------------------------------------------------
    // ActiveTurnRegistry — in-memory in-progress visibility (PR B / Plan C)
    // -----------------------------------------------------------------

    fn mk_tracker_with_registry() -> (TurnTracker, crate::model::ActiveTurnRegistry) {
        let reg = crate::model::new_active_turn_registry();
        let t =
            TurnTracker::with_registry(TrackerConfig::default(), test_metrics(), Some(reg.clone()));
        (t, reg)
    }

    fn registry_snapshot(reg: &crate::model::ActiveTurnRegistry) -> Vec<AgentTurn> {
        reg.read().unwrap().values().cloned().collect()
    }

    #[test]
    fn registry_holds_in_progress_after_first_call() {
        // First user_start call: tracker mints turn_id, builds an
        // InProgress snapshot, and inserts it into the registry. The
        // API reading the registry sees the turn before any DB write.
        let (mut t, reg) = mk_tracker_with_registry();
        let c1 = anthropic_call("S-reg", 1_000_000, "text", "tool_use");
        let id1 = call_info_for_anthropic(&c1);
        let events = t.ingest(ic(c1, id1));
        assert!(
            drain_completed(events).is_empty(),
            "no terminal landed yet — no Completed event"
        );

        let snaps = registry_snapshot(&reg);
        assert_eq!(snaps.len(), 1, "exactly one in-progress snapshot");
        let s = &snaps[0];
        assert_eq!(s.status, TurnStatus::InProgress);
        assert_eq!(s.call_count, 1);
        assert_eq!(s.session_id, "S-reg");
        assert!(s.final_finish_reason.is_none());
        assert!(s.final_call_id.is_none());
    }

    #[test]
    fn registry_updates_in_place_on_each_ingest() {
        // Subsequent calls in the same turn upsert the SAME registry
        // entry (same turn_id) with the latest cumulative state — the
        // map never grows beyond one entry per session.
        let (mut t, reg) = mk_tracker_with_registry();
        let c1 = anthropic_call("S-upd", 1_000_000, "text", "tool_use");
        let id1 = call_info_for_anthropic(&c1);
        let _ = t.ingest(ic(c1, id1));
        let first_id = registry_snapshot(&reg)[0].turn_id.clone();

        let c2 = anthropic_call("S-upd", 2_000_000, "tool_result", "tool_use");
        let id2 = call_info_for_anthropic(&c2);
        let _ = t.ingest(ic(c2, id2));
        let c3 = anthropic_call("S-upd", 3_000_000, "tool_result", "tool_use");
        let id3 = call_info_for_anthropic(&c3);
        let _ = t.ingest(ic(c3, id3));

        let snaps = registry_snapshot(&reg);
        assert_eq!(snaps.len(), 1, "one entry per session, upserted in place");
        let s = &snaps[0];
        assert_eq!(s.turn_id, first_id, "turn_id stable across ingests");
        assert_eq!(s.call_count, 3, "snapshot reflects cumulative state");
        assert_eq!(s.status, TurnStatus::InProgress);
    }

    #[test]
    fn registry_drops_entry_after_grace_completes_turn() {
        // Once the terminal call lands and grace expires, the persisted
        // Completed event reuses the in-progress turn_id, AND the
        // registry entry is removed so the API doesn't double-count
        // the turn (one in-memory + one in DuckDB).
        let (mut t, reg) = mk_tracker_with_registry();
        let now = std::time::Instant::now();

        let c1 = anthropic_call("S-grace", 1_000_000, "text", "tool_use");
        let id1 = call_info_for_anthropic(&c1);
        let _ = t.ingest_at(ic(c1, id1), now);
        let in_progress_id = registry_snapshot(&reg)[0].turn_id.clone();
        assert_eq!(registry_snapshot(&reg).len(), 1);

        let c2 = anthropic_call("S-grace", 2_000_000, "tool_result", "end_turn");
        let id2 = call_info_for_anthropic(&c2);
        let _ = t.ingest_at(ic(c2, id2), now + Duration::from_millis(50));

        // Force grace expiry.
        let later = now + Duration::from_secs(2);
        let events = t.flush_all_at(later);
        let completed = drain_completed(events);
        assert_eq!(completed.len(), 1, "exactly one Completed");
        assert_eq!(
            completed[0].turn_id, in_progress_id,
            "Completed reuses in-progress turn_id (UI sees in-place transition)"
        );
        assert_eq!(completed[0].status, TurnStatus::Complete);
        assert!(
            registry_snapshot(&reg).is_empty(),
            "registry entry must be removed once Completed lands in DB"
        );
    }

    #[test]
    fn registry_drops_entry_after_idle_sweep() {
        // Idle path: no terminal arrives, but `idle_timeout_us` of
        // event-time inactivity passes. Sweep emits Completed/Incomplete
        // and the registry entry is removed so the user sees the row
        // flip in place from in_progress to incomplete (rather than the
        // in-progress row hanging around with the same turn_id as a
        // newly-persisted incomplete row).
        let cfg = TrackerConfig {
            idle_timeout_us: 1_000_000,
            sweep_interval_us: 100_000,
            grace: Duration::from_millis(1_000),
        };
        let reg = crate::model::new_active_turn_registry();
        let mut t = TurnTracker::with_registry(cfg, test_metrics(), Some(reg.clone()));
        let now = std::time::Instant::now();

        let c1 = anthropic_call("S-idle", 1_000_000, "text", "tool_use");
        let id1 = call_info_for_anthropic(&c1);
        let _ = t.ingest_at(ic(c1, id1), now);
        let in_progress_id = registry_snapshot(&reg)[0].turn_id.clone();

        let events = t.advance_time_at(3_500_000, "", now);
        let completed = drain_completed(events);
        assert_eq!(completed.len(), 1, "idle sweep emits exactly one Completed");
        assert_eq!(completed[0].turn_id, in_progress_id);
        assert_eq!(completed[0].status, TurnStatus::Incomplete);
        assert!(
            registry_snapshot(&reg).is_empty(),
            "idle sweep must drop the in-progress registry entry"
        );
    }

    #[test]
    fn registry_skips_subagent_only_partition() {
        // The discard rule (no main-agent user_turn_start → drop on
        // finalize) must apply at registry-insert time too — otherwise
        // the API would briefly show a phantom in-progress row that
        // never makes it to the DB. We synthesize the case by feeding
        // a tool_result first (continuation, no user_start) and asserting
        // the registry stays empty until a user_start call arrives.
        let (mut t, reg) = mk_tracker_with_registry();
        // tool_result body ⇒ is_user_turn_start = false on main agent.
        let c1 = anthropic_call("S-no-start", 1_000_000, "tool_result", "tool_use");
        let id1 = call_info_for_anthropic(&c1);
        let _ = t.ingest(ic(c1, id1));
        assert!(
            registry_snapshot(&reg).is_empty(),
            "no user_start ⇒ registry stays empty"
        );

        // user_start arrives — registry now populates.
        let c2 = anthropic_call("S-no-start", 2_000_000, "text", "tool_use");
        let id2 = call_info_for_anthropic(&c2);
        let _ = t.ingest(ic(c2, id2));
        let snaps = registry_snapshot(&reg);
        assert_eq!(snaps.len(), 1);
        assert_eq!(
            snaps[0].call_count, 2,
            "snapshot includes both buffered calls"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn registry_handles_100_sessions_no_panics() {
        // Stress test (Plan C analogue of the original DB-upsert test):
        // 100 distinct sessions, each ingesting 50 calls in parallel
        // tasks, all sharing the SAME tracker behind a Mutex. The
        // registry's RwLock + the tracker's serialized ingest path must
        // not panic, deadlock, or leak entries.
        //
        // Why a Mutex around the tracker rather than 100 trackers: the
        // production wiring runs ONE TurnTracker per shard, sharded by
        // session_id, so a single tracker really does see all sessions.
        // 100 concurrent ingests through a Mutex models that fan-in
        // worst case.
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        let (tracker, reg) = mk_tracker_with_registry();
        let tracker = StdArc::new(StdMutex::new(tracker));

        const N_SESSIONS: usize = 100;
        const ITERS: usize = 50;

        let mut joins = Vec::with_capacity(N_SESSIONS);
        for s in 0..N_SESSIONS {
            let tracker = tracker.clone();
            joins.push(tokio::spawn(async move {
                let session = format!("S-stress-{s}");
                for k in 0..ITERS {
                    // Each call needs a strictly-increasing request_time
                    // within its session so the orphan guard doesn't
                    // drop later arrivals.
                    let body = if k == 0 { "text" } else { "tool_result" };
                    let ts_us = 1_000_000 + (s as i64) * 10_000_000 + k as i64;
                    let call = anthropic_call(&session, ts_us, body, "tool_use");
                    let id = call_info_for_anthropic(&call);
                    {
                        let mut tr = tracker.lock().unwrap();
                        let _ = tr.ingest(ic(call, id));
                    }
                    // Yield so other tasks can interleave.
                    tokio::task::yield_now().await;
                }
            }));
        }
        for j in joins {
            j.await.unwrap();
        }

        // Every session is in-progress (no terminals fired) → registry
        // should have exactly 100 entries, each with call_count = ITERS.
        let snaps = registry_snapshot(&reg);
        assert_eq!(
            snaps.len(),
            N_SESSIONS,
            "exactly one in-progress entry per session"
        );
        assert!(
            snaps.iter().all(|s| s.call_count == ITERS as u32),
            "every snapshot reflects the full per-session ingest count"
        );
        assert!(
            snaps.iter().all(|s| s.status == TurnStatus::InProgress),
            "every snapshot is in_progress"
        );

        // Finalize all of them via flush_all and confirm registry empties.
        let mut tr = tracker.lock().unwrap();
        let events = tr.flush_all_at(std::time::Instant::now() + Duration::from_secs(2));
        let completed = drain_completed(events);
        assert_eq!(
            completed.len(),
            N_SESSIONS,
            "every session emits one Completed (Incomplete, no terminals)"
        );
        assert!(
            registry_snapshot(&reg).is_empty(),
            "registry must be drained after flush_all"
        );
    }
}
