//! Buffer-and-finalize turn tracker. See `docs/design/04b-turn-reorder-proposal.md`.
//!
//! Each `(stream_id, session_id)` owns a `SessionBuffer` that holds calls
//! sorted by `request_time` until a main-agent terminal call appears and its
//! grace window elapses. On grace expiry the buffer is partitioned at each
//! terminal and every partition becomes one `LlmTurn`. Partitions that
//! contain no `is_user_turn_start = Some(true)` call are discarded.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use uuid::Uuid;

use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_llm::model::{FinishReason, IdentifiedCall};
use ts_llm::profile::{ClientProfile, ProfileRegistry};

use crate::model::{LlmTurn, TurnStatus};

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

/// Tracker configuration. Timestamps are in microseconds (matching `LlmCall.request_time`).
#[derive(Debug, Clone, Copy)]
pub struct TrackerConfig {
    pub idle_timeout_us: i64,
    pub sweep_interval_us: i64,
    /// Wait this long after a terminal call lands before partitioning the
    /// session buffer. Covers fan-in jitter from same-session calls riding
    /// on different TCP connections / different llm-stage workers.
    pub grace_us: i64,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_us: 600_000_000, // 600 s
            sweep_interval_us: 10_000_000, // 10 s
            grace_us: 1_000_000,           // 1 s
        }
    }
}

/// Tracker output. Only finalized turns are emitted; per-call lifecycle
/// events were removed when the buffer model replaced ActiveTurn (04b §6.4).
#[derive(Debug, Clone)]
pub enum TurnEvent {
    Completed(LlmTurn),
}

#[derive(Debug)]
struct BufferedCall {
    ic: IdentifiedCall,
    /// `virtual_now_us` at the moment ingest stored this call. Each
    /// terminal's grace is checked against its own arrival.
    arrived_at_us: i64,
    /// Cached `is_main_terminal` so finalize doesn't re-resolve per element.
    is_terminal: bool,
}

#[derive(Default, Debug)]
struct SessionBuffer {
    /// Calls awaiting partition, ordered by `request_time`. The Vec at each
    /// key handles request_time collisions in insertion order.
    pending: BTreeMap<i64, Vec<BufferedCall>>,
    /// `arrived_at_us` of the earliest pending terminal. `None` ⇒ no
    /// terminal currently pending. Set by ingest; reseated by finalize after
    /// each partition emission.
    grace_started_at_us: Option<i64>,
    /// Largest `request_time` ever included in a finalized (or discarded)
    /// partition for this session. New arrivals strictly older than this are
    /// orphaned at the entry guard.
    last_finalized_request_time: Option<i64>,
    /// Latest `virtual_now_us` observed for this buffer. Used as the idle
    /// reference by `sweep`.
    last_activity_us: i64,
}

/// Single stateful owner of turn assembly. Passive: callers drive it via
/// `ingest`, `advance_time`, `sweep`, `flush_all`.
pub struct TurnTracker {
    registry: Arc<ProfileRegistry>,
    config: TrackerConfig,
    buffers: HashMap<(String, String), SessionBuffer>,
    virtual_now_us: i64,
    last_sweep_us: i64,
    metrics: MetricsWorker,
}

impl TurnTracker {
    pub fn new(
        registry: Arc<ProfileRegistry>,
        config: TrackerConfig,
        metrics: MetricsWorker,
    ) -> Self {
        Self {
            registry,
            config,
            buffers: HashMap::new(),
            virtual_now_us: 0,
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

    pub fn virtual_now_us(&self) -> i64 {
        self.virtual_now_us
    }

    /// Ingest one identified, completed call. Returns any turns whose
    /// grace expired as a result of advancing virtual time to this call.
    pub fn ingest(&mut self, ic: IdentifiedCall) -> Vec<TurnEvent> {
        let arrival_ts = ic
            .call
            .complete_time
            .or(ic.call.response_time)
            .unwrap_or(ic.call.request_time);
        self.virtual_now_us = self.virtual_now_us.max(arrival_ts);
        self.metrics.counter(Metric::TurnCallsIngested).inc();

        let registry = Arc::clone(&self.registry);
        let profile = match registry.find_by_name(ic.identity.profile_name) {
            Some(p) => p,
            None => return self.flush_ready_buffers(),
        };

        // Auxiliary one-shots (e.g., claude-cli session-title) bypass turn
        // assembly entirely. They still flow to storage independently.
        if profile.is_auxiliary(&ic.call) {
            self.metrics.counter(Metric::TurnCallsAuxiliary).inc();
            return self.flush_ready_buffers();
        }

        let key = (ic.call.stream_id.clone(), ic.identity.session_id.clone());
        let buf = self.buffers.entry(key).or_default();

        if let Some(hw) = buf.last_finalized_request_time {
            if ic.call.request_time < hw {
                self.metrics.counter(Metric::TurnReorderOrphan).inc();
                return self.flush_ready_buffers();
            }
        }

        let is_terminal = is_main_terminal(profile, &ic);
        let request_time = ic.call.request_time;
        let virtual_now = self.virtual_now_us;
        buf.pending
            .entry(request_time)
            .or_default()
            .push(BufferedCall {
                ic,
                arrived_at_us: virtual_now,
                is_terminal,
            });
        buf.last_activity_us = virtual_now;
        if is_terminal && buf.grace_started_at_us.is_none() {
            buf.grace_started_at_us = Some(virtual_now);
        }

        self.flush_ready_buffers()
    }

    /// Walk every buffer; for each whose front-pending terminal's grace has
    /// expired, hand off to `finalize_session` to emit one or more turns.
    fn flush_ready_buffers(&mut self) -> Vec<TurnEvent> {
        let now_us = self.virtual_now_us;
        let grace_us = self.config.grace_us;
        let registry = Arc::clone(&self.registry);

        let ready_keys: Vec<(String, String)> = self
            .buffers
            .iter()
            .filter_map(|(k, b)| match b.grace_started_at_us {
                Some(started) if now_us >= started + grace_us => Some(k.clone()),
                _ => None,
            })
            .collect();

        let mut events = Vec::new();
        for key in ready_keys {
            let profile_name = match self
                .buffers
                .get(&key)
                .and_then(|b| b.pending.values().flatten().next())
                .map(|bc| bc.ic.identity.profile_name)
            {
                Some(n) => n,
                None => continue,
            };
            let profile = match registry.find_by_name(profile_name) {
                Some(p) => p,
                None => continue,
            };
            let buf = self.buffers.get_mut(&key).expect("key just listed");
            finalize_session(
                profile,
                &key.0,
                &key.1,
                buf,
                now_us,
                grace_us,
                &self.metrics,
                &mut events,
            );
        }
        self.gc_buffers();
        events
    }

    /// Drop fully-drained buffers whose `last_activity_us` is well past the
    /// idle horizon. Loses the orphan guard for that session, which is fine
    /// past `2 × idle_timeout_us` — far longer than any plausible reorder.
    fn gc_buffers(&mut self) {
        let cutoff = self
            .virtual_now_us
            .saturating_sub(2 * self.config.idle_timeout_us);
        self.buffers.retain(|_, b| {
            !(b.pending.is_empty()
                && b.last_finalized_request_time.is_some()
                && b.last_activity_us < cutoff)
        });
    }

    /// Advance virtual time using an external signal (e.g., a heartbeat
    /// forwarded through the pipeline) and run any consequent finalize/sweep.
    pub fn advance_time(&mut self, ts: i64) -> Vec<TurnEvent> {
        self.virtual_now_us = self.virtual_now_us.max(ts);
        let mut events = self.flush_ready_buffers();
        events.extend(self.sweep());
        events
    }

    /// Idle fallback: drain buffers that hold no terminal call and whose
    /// newest call is older than `idle_timeout_us`. Discard rule applies.
    pub fn sweep(&mut self) -> Vec<TurnEvent> {
        if self.virtual_now_us - self.last_sweep_us < self.config.sweep_interval_us {
            return Vec::new();
        }
        self.last_sweep_us = self.virtual_now_us;
        let cutoff = self.virtual_now_us - self.config.idle_timeout_us;
        let registry = Arc::clone(&self.registry);

        let candidates: Vec<(String, String)> = self
            .buffers
            .iter()
            .filter(|(_, b)| {
                !b.pending.is_empty()
                    && !b.pending.values().flatten().any(|bc| bc.is_terminal)
                    && b.last_activity_us < cutoff
            })
            .map(|(k, _)| k.clone())
            .collect();

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
            buf.grace_started_at_us = None;

            let profile_name = drained[0].ic.identity.profile_name;
            let profile = match registry.find_by_name(profile_name) {
                Some(p) => p,
                None => continue,
            };

            emit_or_discard(
                profile,
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

    /// EOF / shutdown drain. Force every pending terminal past its grace,
    /// then emit any remaining non-terminal tail (subject to discard rule).
    pub fn flush_all(&mut self) -> Vec<TurnEvent> {
        let registry = Arc::clone(&self.registry);
        let mut keys: Vec<(String, String)> = self.buffers.keys().cloned().collect();
        keys.sort();
        let force_now = self
            .virtual_now_us
            .saturating_add(self.config.grace_us)
            .saturating_add(1);
        let grace_us = self.config.grace_us;

        let mut events = Vec::new();
        for key in keys {
            let profile_name = match self
                .buffers
                .get(&key)
                .and_then(|b| b.pending.values().flatten().next())
                .map(|bc| bc.ic.identity.profile_name)
            {
                Some(n) => n,
                None => continue,
            };
            let profile = match registry.find_by_name(profile_name) {
                Some(p) => p,
                None => continue,
            };

            // Step 1: drive the terminal-bounded partitions to finalize.
            {
                let buf = self.buffers.get_mut(&key).expect("listed above");
                finalize_session(
                    profile,
                    &key.0,
                    &key.1,
                    buf,
                    force_now,
                    grace_us,
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
            buf.grace_started_at_us = None;
            emit_or_discard(
                profile,
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
/// reseats `buf.grace_started_at_us` to that next terminal's arrival.
#[allow(clippy::too_many_arguments)]
fn finalize_session(
    profile: &dyn ClientProfile,
    stream_id: &str,
    session_id: &str,
    buf: &mut SessionBuffer,
    now_us: i64,
    grace_us: i64,
    metrics: &MetricsWorker,
    events: &mut Vec<TurnEvent>,
) {
    loop {
        let mut sorted: Vec<&BufferedCall> = buf.pending.values().flatten().collect();
        sorted.sort_by_key(|bc| bc.ic.call.request_time);
        if sorted.is_empty() {
            buf.grace_started_at_us = None;
            return;
        }

        let terminal_idx = sorted.iter().position(|bc| bc.is_terminal);
        let idx = match terminal_idx {
            None => {
                buf.grace_started_at_us = None;
                return;
            }
            Some(i) => i,
        };
        let front_arrived = sorted[idx].arrived_at_us;
        if now_us < front_arrived + grace_us {
            // Reseat grace clock to *this* terminal's arrival — earlier
            // terminals may already have been consumed in prior iterations.
            buf.grace_started_at_us = Some(front_arrived);
            return;
        }

        let terminal_ts = sorted[idx].ic.call.request_time;
        // Partition: every pending key ≤ terminal_ts → this turn.
        let turn_keys: Vec<i64> = buf
            .pending
            .range(..=terminal_ts)
            .map(|(k, _)| *k)
            .collect();
        let mut turn_calls: Vec<BufferedCall> = Vec::new();
        for k in turn_keys {
            if let Some(v) = buf.pending.remove(&k) {
                turn_calls.extend(v);
            }
        }

        emit_or_discard(
            profile,
            stream_id,
            session_id,
            &turn_calls,
            metrics,
            events,
            FinalizeKind::Flush, // grace-driven; counter chosen below by caller wrapper
        );
        // The wrapper above always counts TurnsCompleted; for the grace
        // path we additionally bump the grace-specific counter so operators
        // can tell grace from idle from EOF in the time series.
        // (Idle and EOF call sites already bump their own counters.)
        if matches!(events.last(), Some(TurnEvent::Completed(_))) {
            metrics.counter(Metric::TurnFinalizedByGrace).inc();
        }

        buf.last_finalized_request_time = Some(terminal_ts);
        // Loop: the next iteration recomputes the next pending terminal.
    }
}

/// Apply the discard rule and emit (or count-and-drop) one turn per drained
/// partition. The caller has already removed `calls` from the buffer and
/// updated bookkeeping.
fn emit_or_discard(
    profile: &dyn ClientProfile,
    stream_id: &str,
    session_id: &str,
    calls: &[BufferedCall],
    metrics: &MetricsWorker,
    events: &mut Vec<TurnEvent>,
    kind: FinalizeKind,
) {
    if calls.is_empty() {
        return;
    }
    let has_user_start = calls
        .iter()
        .any(|bc| profile.is_user_turn_start(&bc.ic.call) == Some(true));
    if !has_user_start {
        metrics.counter(Metric::TurnDiscardedNoUserStart).inc();
        return;
    }

    let refs: Vec<&IdentifiedCall> = calls.iter().map(|bc| &bc.ic).collect();
    let status = derive_status(profile, &refs);
    let mut turn = build_turn(profile, &refs, status);
    // The buffer key is authoritative for stream/session — call.stream_id
    // can legitimately be empty in tests; identity.session_id may differ
    // from a future cross-bucket scheme. Using the key keeps us consistent.
    turn.stream_id = stream_id.to_string();
    turn.session_id = session_id.to_string();
    events.push(TurnEvent::Completed(turn));
    metrics.counter(Metric::TurnsCompleted).inc();
    if matches!(kind, FinalizeKind::Idle) {
        metrics.counter(Metric::TurnFinalizedByIdle).inc();
        metrics.counter(Metric::TurnsTimedOut).inc();
    }
}

/// "Does this call end the main agent's turn?"
///
/// Sub-agent calls are excluded outright. For profiles that provide an
/// explicit `turn_id_hint` (Codex), only the profile's own `is_turn_terminal`
/// predicate counts — Codex's `finish_reason=Complete` means "API call
/// succeeded," not "turn done." For implicit-path profiles (Anthropic),
/// definitive finish reasons fall through.
fn is_main_terminal(profile: &dyn ClientProfile, ic: &IdentifiedCall) -> bool {
    if profile.subagent(&ic.call).is_some() {
        return false;
    }
    if profile.is_turn_terminal(&ic.call) {
        return true;
    }
    if ic.identity.turn_id_hint.is_none() {
        matches!(
            ic.call.finish_reason,
            Some(FinishReason::Complete) | Some(FinishReason::Length)
        )
    } else {
        false
    }
}

/// Map the last main-agent call's finish_reason to a TurnStatus. Sub-agent
/// finishes are ignored — they belong to the sub-agent, not the parent turn.
fn derive_status(profile: &dyn ClientProfile, calls: &[&IdentifiedCall]) -> TurnStatus {
    let last_main = calls
        .iter()
        .rev()
        .find(|ic| profile.subagent(&ic.call).is_none());
    match last_main.and_then(|ic| ic.call.finish_reason) {
        Some(FinishReason::Complete) => TurnStatus::Complete,
        Some(FinishReason::Length) => TurnStatus::Length,
        Some(FinishReason::Cancelled) => TurnStatus::Cancelled,
        Some(FinishReason::Error) => TurnStatus::Failed,
        _ => TurnStatus::Incomplete,
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.iter().any(|v| v == &value) {
        list.push(value);
    }
}

/// Pure constructor over a request_time-sorted, complete partition.
fn build_turn(
    profile: &dyn ClientProfile,
    calls: &[&IdentifiedCall],
    status: TurnStatus,
) -> LlmTurn {
    assert!(!calls.is_empty(), "build_turn requires at least one call");
    let first = calls[0];
    let stream_id = first.call.stream_id.clone();
    let session_id = first.identity.session_id.clone();
    let client_kind = first.identity.client_kind.clone();
    let provider = first.call.provider.to_string();
    let tenant_id = first.call.tenant_id.clone();
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

    // final_*: the last main-agent call by request_time. Sub-agent text
    // belongs to the sub-agent, never to the parent's final answer.
    let last_main = calls
        .iter()
        .rev()
        .find(|ic| profile.subagent(&ic.call).is_none());
    let (final_answer_preview, final_call_id, final_finish_reason) = match last_main {
        Some(ic) => {
            let preview = profile
                .extract_assistant_text(&ic.call)
                .map(|t| truncate_preview(&t, FINAL_ANSWER_PREVIEW_CHARS));
            (
                preview,
                Some(ic.call.id.clone()),
                ic.call.finish_reason.map(|r| r.to_string()),
            )
        }
        None => (None, None, None),
    };

    LlmTurn {
        stream_id,
        turn_id,
        session_id,
        tenant_id,
        provider,
        client_kind,
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
    use ts_llm::model::{ApiType, CallIdentity, IdentifiedCall, LlmCall, ProviderFormat};
    use ts_llm::profiles;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
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

    /// Build a tracker with `grace_us=0` so calls finalize on the very
    /// virtual_now tick they arrive at — keeps unit assertions tight.
    fn mk_tracker_no_grace() -> TurnTracker {
        TurnTracker::new(
            Arc::new(profiles::build_default_registry()),
            TrackerConfig {
                grace_us: 0,
                ..TrackerConfig::default()
            },
            test_metrics(),
        )
    }

    fn ic(call: LlmCall, identity: CallIdentity) -> IdentifiedCall {
        IdentifiedCall {
            call: Arc::new(call),
            identity,
        }
    }

    fn identity_for_anthropic(call: &LlmCall) -> CallIdentity {
        let reg = profiles::build_default_registry();
        let profile = reg.find_by_name("claude-cli").expect("claude-cli profile");
        let ids = profile.extract_ids(call).expect("ids");
        CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".into(),
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        }
    }

    fn identity_for_codex(call: &LlmCall) -> CallIdentity {
        let reg = profiles::build_default_registry();
        let profile = reg.find_by_name("codex-cli").expect("codex-cli profile");
        let ids = profile.extract_ids(call).expect("ids");
        CallIdentity {
            profile_name: "codex-cli",
            client_kind: "codex-cli".into(),
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        }
    }

    fn anthropic_call(
        session: &str,
        request_time_us: i64,
        body_kind: &str,
        finish: FinishReason,
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
            response_body: None,
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

    fn codex_call(
        session: &str,
        turn: &str,
        body_input_type: &str,
        finish: FinishReason,
    ) -> LlmCall {
        let meta = format!(r#"{{"session_id":"{session}","turn_id":"{turn}"}}"#);
        let body = match body_input_type {
            "message" => {
                r#"{"input":[{"type":"message","role":"user","content":"hi"}]}"#.to_string()
            }
            other => format!(r#"{{"input":[{{"type":"{other}"}}]}}"#),
        };
        LlmCall {
            stream_id: String::new(),
            id: format!("c-{turn}"),
            provider: ProviderFormat::OpenAIResponses,
            model: "gpt-5.4".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 1_000_000,
            response_time: Some(1_500_000),
            complete_time: Some(2_000_000),
            request_path: "/v1/responses".into(),
            is_stream: true,
            request_body: Some(body),
            status_code: Some(200),
            finish_reason: Some(finish),
            response_body: None,
            input_tokens: Some(100),
            output_tokens: Some(10),
            total_tokens: Some(110),
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
                ("Originator".into(), "codex_cli_rs".into()),
                ("X-Codex-Turn-Metadata".into(), meta),
            ],
            response_headers: vec![],
        }
    }

    fn drain_completed(events: Vec<TurnEvent>) -> Vec<LlmTurn> {
        events
            .into_iter()
            .map(|TurnEvent::Completed(t)| t)
            .collect()
    }

    #[test]
    fn tracker_starts_empty() {
        let t = TurnTracker::new(
            Arc::new(ProfileRegistry::new()),
            TrackerConfig::default(),
            test_metrics(),
        );
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_on_empty_tracker_returns_no_events() {
        let mut t = TurnTracker::new(
            Arc::new(ProfileRegistry::new()),
            TrackerConfig::default(),
            test_metrics(),
        );
        assert!(t.flush_all().is_empty());
    }

    #[test]
    fn anthropic_two_call_turn_finalizes_after_grace() {
        // c1 (user-start, ToolUse) → c2 (cont, Complete = main terminal).
        // grace_us=0 → c2 grace expires immediately on its own ingest.
        let mut t = mk_tracker_no_grace();
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete);
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
        let mut c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        c1.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>ignore</system-reminder>plan the refactor"}]}],"tools":[{"name":"Agent"},{"name":"Bash"}]}"#.into(),
        );
        let mut c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete);
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
        assert_eq!(turn.user_input_preview.as_deref(), Some("plan the refactor"));
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
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let mut c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete);
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
        let mut c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        c1.request_body = Some(body);
        let c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete);
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
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let mut c2 = anthropic_call("S", 2_000_000, "text", FinishReason::Complete);
        c2.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],"tools":[{"name":"Read"},{"name":"Grep"}]}"#.into(),
        );
        let c3 = anthropic_call("S", 3_000_000, "tool_result", FinishReason::ToolUse);
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        let id3 = identity_for_anthropic(&c3);
        let mut events = t.ingest(ic(c1, id1));
        events.extend(t.ingest(ic(c2, id2)));
        events.extend(t.ingest(ic(c3, id3)));
        assert!(drain_completed(events).is_empty(), "no main-agent terminal yet");
        assert_eq!(t.active_count(), 3);
    }

    #[test]
    fn subagent_assistant_text_does_not_leak_to_parent_final_answer() {
        let mut t = mk_tracker_no_grace();
        let mut c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        c1.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"start"}]}],"tools":[{"name":"Agent"}]}"#.into(),
        );
        c1.response_body = Some(r#"{"content":[{"type":"text","text":"parent progress"}]}"#.into());
        let mut c2 = anthropic_call("S", 2_000_000, "text", FinishReason::Complete);
        c2.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"sub task"}]}],"tools":[{"name":"Read"}]}"#.into(),
        );
        c2.response_body =
            Some(r#"{"content":[{"type":"text","text":"sub-agent conclusion"}]}"#.into());
        let mut c3 = anthropic_call("S", 3_000_000, "tool_result", FinishReason::Complete);
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
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::ToolUse);
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
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::Complete);
        let id = identity_for_anthropic(&c);
        let events = t.ingest(ic(c, id));
        let turns = drain_completed(events);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Complete);
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn single_call_length_closes_after_grace() {
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::Length);
        let id = identity_for_anthropic(&c);
        let turns = drain_completed(t.ingest(ic(c, id)));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Length);
    }

    #[test]
    fn single_call_error_stays_buffered() {
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::Error);
        let id = identity_for_anthropic(&c);
        let events = t.ingest(ic(c, id));
        assert!(drain_completed(events).is_empty(), "Error is not terminal");
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn auxiliary_call_is_skipped_entirely() {
        let mut t = mk_tracker_no_grace();
        let mut c = anthropic_call("S", 1_000_000, "text", FinishReason::Complete);
        c.request_body = Some(
            r#"{"messages":[{"role":"user","content":"generate title"}],"tools":[]}"#.into(),
        );
        let id = CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".into(),
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
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let id = identity_for_anthropic(&c);
        t.ingest(ic(c, id));
        assert_eq!(t.active_count(), 1);
        let turns = drain_completed(t.flush_all());
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Incomplete);
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_discards_partition_without_user_start() {
        // A lone continuation: terminal but no user-start → discard, not emit.
        let mut t = mk_tracker_no_grace();
        let c = anthropic_call("S", 1_000_000, "tool_result", FinishReason::Complete);
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
        let c = codex_call("s1", "t1", "message", FinishReason::Complete);
        let id = identity_for_codex(&c);
        let events = t.ingest(ic(c, id));
        assert!(drain_completed(events).is_empty());
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn codex_terminal_output_closes_after_grace() {
        let mut t = mk_tracker_no_grace();
        let mut c = codex_call("s1", "t1", "message", FinishReason::Complete);
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
        let mut c = codex_call("s1", "t1", "message", FinishReason::Complete);
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
    fn sweep_idle_buffer_emits_incomplete_with_user_start() {
        let cfg = TrackerConfig {
            grace_us: 0,
            idle_timeout_us: 1_000,
            sweep_interval_us: 1_000,
        };
        let mut t = TurnTracker::new(
            Arc::new(profiles::build_default_registry()),
            cfg,
            test_metrics(),
        );
        // Non-terminal call (ToolUse) — sits in buffer with no grace.
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let id = identity_for_anthropic(&c);
        let arrival = c.complete_time.unwrap();
        t.ingest(ic(c, id));
        assert_eq!(t.active_count(), 1);

        let swept = drain_completed(t.advance_time(arrival + 1_000_000));
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].status, TurnStatus::Incomplete);
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn ingest_populates_call_ids_into_finalized_turn() {
        let mut t = mk_tracker_no_grace();
        // Two codex calls, second is terminal-output → grace fires.
        let mut c1 = codex_call("s1", "t1", "message", FinishReason::Complete);
        c1.id = "c1".into();
        c1.request_time = 1_000_000;
        c1.complete_time = Some(1_500_000);
        let mut c2 = codex_call("s1", "t1", "message", FinishReason::Complete);
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
        let c1 = anthropic_call("S", 2_000_000, "text", FinishReason::ToolUse);
        let c2 = anthropic_call("S", 3_000_000, "tool_result", FinishReason::Complete);
        let id1 = identity_for_anthropic(&c1);
        let id2 = identity_for_anthropic(&c2);
        t.ingest(ic(c1, id1));
        let _ = t.ingest(ic(c2, id2));
        // Now last_finalized_request_time = 3_000_000 for this session.
        let c0 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
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
