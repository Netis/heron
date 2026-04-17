use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use ts_llm::model::{FinishReason, LlmCall};
use uuid::Uuid;

use crate::model::{LlmTurn, TurnKey, TurnStatus};
use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_llm::profile::{ClientProfile, ProfileRegistry};

/// Max length of final_answer_preview stored on LlmTurn. Longer assistant text
/// lives in full on the llm_calls row pointed at by final_call_id.
const FINAL_ANSWER_PREVIEW_CHARS: usize = 500;

/// Max length of user_input_preview stored on LlmTurn. Longer user prompts
/// live in full on the llm_calls row pointed at by user_call_id.
const USER_INPUT_PREVIEW_CHARS: usize = 500;

fn truncate_preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Tracker configuration. Timestamps are in microseconds (matching LlmCall.request_time).
#[derive(Debug, Clone, Copy)]
pub struct TrackerConfig {
    pub idle_timeout_us: i64,
    pub sweep_interval_us: i64,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_us: 600_000_000,  // 600 s
            sweep_interval_us: 10_000_000, // 10 s (used by the caller; tracker itself is passive)
        }
    }
}

/// Output of the tracker.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    Started {
        key: TurnKey,
        start_time_us: i64,
    },
    CallAdded {
        key: TurnKey,
        call_id: String,
        sequence: u32,
    },
    Completed(LlmTurn),
}

/// In-memory aggregator for one active turn. Not exposed publicly.
#[derive(Debug)]
struct ActiveTurn {
    key: TurnKey,
    tenant_id: Option<String>,
    provider: String,
    client_kind: String,
    start_time_us: i64,
    last_activity_us: i64,
    call_count: u32,
    call_ids: Vec<String>,
    models_used: Vec<String>,
    subagents_used: Vec<String>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_input_tokens: u64,
    total_cache_creation_input_tokens: u64,
    last_finish_reason: Option<FinishReason>,
    user_input_preview: Option<String>,
    user_call_id: Option<String>,
    final_answer_preview: Option<String>,
    final_call_id: Option<String>,
}

impl ActiveTurn {
    fn push_unique(list: &mut Vec<String>, value: String) {
        if !list.iter().any(|v| v == &value) {
            list.push(value);
        }
    }

    fn merge(&mut self, profile: &dyn ClientProfile, call: &LlmCall, subagent: Option<String>) {
        self.last_activity_us = call
            .complete_time
            .or(call.response_time)
            .unwrap_or(call.request_time);
        self.call_count += 1;
        self.call_ids.push(call.id.clone());
        Self::push_unique(&mut self.models_used, call.model.clone());
        let is_subagent = subagent.is_some();
        if let Some(sa) = subagent {
            Self::push_unique(&mut self.subagents_used, sa);
        }
        if let Some(t) = call.input_tokens {
            self.total_input_tokens += t as u64;
        }
        if let Some(t) = call.output_tokens {
            self.total_output_tokens += t as u64;
        }
        if let Some(t) = call.cache_read_input_tokens {
            self.total_cache_read_input_tokens += t as u64;
        }
        if let Some(t) = call.cache_creation_input_tokens {
            self.total_cache_creation_input_tokens += t as u64;
        }
        // Sub-agent calls complete independently of the parent turn — their
        // finish_reason and assistant text belong to the sub-agent, not the
        // main agent. Letting them overwrite the parent's terminal state
        // would prematurely close the turn and attribute the sub-agent's
        // final answer to the main agent. Main-agent calls update as usual.
        if !is_subagent {
            self.last_finish_reason = call.finish_reason;
            self.final_call_id = Some(call.id.clone());
            if let Some(text) = profile.extract_assistant_text(call) {
                self.final_answer_preview =
                    Some(truncate_preview(&text, FINAL_ANSWER_PREVIEW_CHARS));
            }
        }
    }

    fn finalize(self, status: TurnStatus) -> LlmTurn {
        let duration_ms = ((self.last_activity_us - self.start_time_us).max(0) / 1000) as u64;
        LlmTurn {
            stream_id: self.key.stream_id,
            turn_id: self.key.turn_id,
            session_id: self.key.session_id,
            tenant_id: self.tenant_id,
            provider: self.provider,
            client_kind: self.client_kind,
            start_time_us: self.start_time_us,
            end_time_us: self.last_activity_us,
            duration_ms,
            call_count: self.call_count,
            models_used: self.models_used,
            subagents_used: self.subagents_used,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            total_cache_read_input_tokens: self.total_cache_read_input_tokens,
            total_cache_creation_input_tokens: self.total_cache_creation_input_tokens,
            total_cost_usd: None,
            status,
            final_finish_reason: self.last_finish_reason.map(|r| r.to_string()),
            user_input_preview: self.user_input_preview,
            user_call_id: self.user_call_id,
            final_answer_preview: self.final_answer_preview,
            final_call_id: self.final_call_id,
            call_ids: self.call_ids.clone(),
            metadata: serde_json::json!({}),
        }
    }
}

/// The single stateful owner of turn state. Passive: callers drive it via `ingest` and `sweep`.
pub struct TurnTracker {
    registry: Arc<ProfileRegistry>,
    config: TrackerConfig,
    /// Keyed by TurnKey (session_id, turn_id).
    active: HashMap<TurnKey, ActiveTurn>,
    /// Current virtual time (driven by ingested packet timestamps).
    virtual_now_us: i64,
    /// Timestamp of last sweep tick.
    last_sweep_us: i64,
    metrics: MetricsWorker,
}

impl TurnTracker {
    pub fn new(registry: Arc<ProfileRegistry>, config: TrackerConfig, metrics: MetricsWorker) -> Self {
        Self {
            registry,
            config,
            active: HashMap::new(),
            virtual_now_us: 0,
            last_sweep_us: 0,
            metrics,
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn virtual_now_us(&self) -> i64 {
        self.virtual_now_us
    }

    /// Ingest one completed LlmCall. The call must have been pre-identified
    /// by the upstream stage; `identity` carries the extracted session/turn ids.
    /// Returns TurnEvents in emission order. Does NOT mutate `call`.
    pub fn ingest(
        &mut self,
        call: &LlmCall,
        identity: &ts_llm::model::CallIdentity,
    ) -> Vec<TurnEvent> {
        self.virtual_now_us = self.virtual_now_us.max(
            call.complete_time
                .or(call.response_time)
                .unwrap_or(call.request_time),
        );
        self.metrics.counter(Metric::TurnCallsIngested).inc();
        let profile = match self.registry.find_by_name(identity.profile_name) {
            Some(p) => p,
            None => return Vec::new(),
        };

        // Auxiliary calls (e.g., claude-cli session-title generation) must not
        // participate in turn tracking: they neither open, close, nor extend a
        // turn. The call record still flows to storage independently.
        if profile.is_auxiliary(call) {
            self.metrics.counter(Metric::TurnCallsAuxiliary).inc();
            return Vec::new();
        }

        let mut events = Vec::new();
        let explicit_turn = identity.turn_id_hint.clone();
        let subagent = profile.subagent(call);
        let is_subagent = subagent.is_some();
        let stream_id = call.stream_id.clone();
        let session_id = identity.session_id.clone();

        // --- Explicit turn_id path (Codex) ---
        if let Some(turn_id) = explicit_turn {
            let key = TurnKey {
                stream_id: stream_id.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            };

            let stale_keys: Vec<TurnKey> = self
                .active
                .keys()
                .filter(|k| k.stream_id == stream_id && k.session_id == session_id && k.turn_id != turn_id)
                .cloned()
                .collect();
            for sk in stale_keys {
                if let Some(at) = self.active.remove(&sk) {
                    events.push(TurnEvent::Completed(at.finalize(TurnStatus::Incomplete)));
                    self.metrics.counter(Metric::TurnsCompleted).inc();
                }
            }

            let is_new = !self.active.contains_key(&key);
            let (initial_user_input_preview, initial_user_call_id) = if is_new {
                match profile.extract_user_input(call) {
                    Some(text) => (
                        Some(truncate_preview(&text, USER_INPUT_PREVIEW_CHARS)),
                        Some(call.id.clone()),
                    ),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
            let at = self
                .active
                .entry(key.clone())
                .or_insert_with(|| ActiveTurn {
                    key: key.clone(),
                    tenant_id: call.tenant_id.clone(),
                    provider: call.provider.to_string(),
                    client_kind: profile.name().to_string(),
                    start_time_us: call.request_time,
                    last_activity_us: call.request_time,
                    call_count: 0,
                    call_ids: Vec::new(),
                    models_used: Vec::new(),
                    subagents_used: Vec::new(),
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    total_cache_read_input_tokens: 0,
                    total_cache_creation_input_tokens: 0,
                    last_finish_reason: None,
                    user_input_preview: initial_user_input_preview,
                    user_call_id: initial_user_call_id,
                    final_answer_preview: None,
                    final_call_id: None,
                });
            if is_new {
                events.push(TurnEvent::Started {
                    key: key.clone(),
                    start_time_us: at.start_time_us,
                });
            }
            at.merge(profile, call, subagent);
            events.push(TurnEvent::CallAdded {
                key: key.clone(),
                call_id: call.id.clone(),
                sequence: at.call_count - 1,
            });

            // Explicit-path immediate close: ask the profile whether this
            // call terminates the agent turn. For Codex this inspects
            // response.output for any *_call item that would force another
            // API roundtrip; absence ⇒ final answer, close now. Sub-agent
            // calls never close the parent turn. Falling through still
            // leaves (1) new turn_id arrival and (2) idle-timeout sweep as
            // backstops.
            if !is_subagent && profile.is_turn_terminal(call) {
                if let Some(at) = self.active.remove(&key) {
                    let status = match at.last_finish_reason {
                        Some(FinishReason::Complete) => TurnStatus::Complete,
                        Some(FinishReason::Length) => TurnStatus::Length,
                        Some(FinishReason::Cancelled) => TurnStatus::Cancelled,
                        Some(FinishReason::Error) => TurnStatus::Failed,
                        _ => TurnStatus::Incomplete,
                    };
                    events.push(TurnEvent::Completed(at.finalize(status)));
                    self.metrics.counter(Metric::TurnsCompleted).inc();
                }
            }

            return events;
        }

        // --- Implicit path (Anthropic) ---
        let is_user_start = profile.is_user_turn_start(call).unwrap_or(false);

        let existing_key: Option<TurnKey> = self
            .active
            .keys()
            .find(|k| k.stream_id == stream_id && k.session_id == session_id)
            .cloned();

        if let Some(ref key) = existing_key {
            let last_finish = self.active.get(key).and_then(|t| t.last_finish_reason);
            let terminal = matches!(
                last_finish,
                Some(FinishReason::Complete)
                    | Some(FinishReason::Length)
                    | Some(FinishReason::Error)
                    | Some(FinishReason::Cancelled)
            );
            if terminal || is_user_start {
                if let Some(at) = self.active.remove(key) {
                    let status = match (terminal, last_finish) {
                        (true, Some(FinishReason::Complete)) => TurnStatus::Complete,
                        (true, Some(FinishReason::Length)) => TurnStatus::Length,
                        (true, Some(FinishReason::Cancelled)) => TurnStatus::Cancelled,
                        (true, Some(FinishReason::Error)) => TurnStatus::Failed,
                        _ => TurnStatus::Incomplete,
                    };
                    events.push(TurnEvent::Completed(at.finalize(status)));
                    self.metrics.counter(Metric::TurnsCompleted).inc();
                }
            }
        }

        let key = match self
            .active
            .keys()
            .find(|k| k.stream_id == stream_id && k.session_id == session_id)
            .cloned()
        {
            Some(k) => k,
            None => {
                let new_turn_id = Uuid::now_v7().to_string();
                TurnKey {
                    stream_id: stream_id.clone(),
                    session_id: session_id.clone(),
                    turn_id: new_turn_id,
                }
            }
        };

        let is_new = !self.active.contains_key(&key);
        let (initial_user_input_preview, initial_user_call_id) = if is_new {
            match profile.extract_user_input(call) {
                Some(text) => (
                    Some(truncate_preview(&text, USER_INPUT_PREVIEW_CHARS)),
                    Some(call.id.clone()),
                ),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let at = self
            .active
            .entry(key.clone())
            .or_insert_with(|| ActiveTurn {
                key: key.clone(),
                tenant_id: call.tenant_id.clone(),
                provider: call.provider.to_string(),
                client_kind: profile.name().to_string(),
                start_time_us: call.request_time,
                last_activity_us: call.request_time,
                call_count: 0,
                call_ids: Vec::new(),
                models_used: Vec::new(),
                subagents_used: Vec::new(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cache_read_input_tokens: 0,
                total_cache_creation_input_tokens: 0,
                last_finish_reason: None,
                user_input_preview: initial_user_input_preview,
                user_call_id: initial_user_call_id,
                final_answer_preview: None,
                final_call_id: None,
            });
        if is_new {
            events.push(TurnEvent::Started {
                key: key.clone(),
                start_time_us: at.start_time_us,
            });
        }
        at.merge(profile, call, subagent);
        events.push(TurnEvent::CallAdded {
            key: key.clone(),
            call_id: call.id.clone(),
            sequence: at.call_count - 1,
        });

        let current_finish = call.finish_reason;
        // Sub-agent finishes are intra-turn events; only main-agent terminal
        // signals close the parent turn.
        let now_terminal = !is_subagent
            && matches!(
                current_finish,
                Some(FinishReason::Complete)
                    | Some(FinishReason::Length)
                    | Some(FinishReason::Error)
                    | Some(FinishReason::Cancelled)
            );
        // Allow immediate close when the turn is new BUT we have high
        // confidence it is a complete single-call turn (user-initiated +
        // definitively terminal). Error/Cancelled stay guarded because the
        // client may retry within the same logical turn.
        let confident_single = is_new
            && is_user_start
            && matches!(
                current_finish,
                Some(FinishReason::Complete) | Some(FinishReason::Length)
            );
        if now_terminal && (!is_new || confident_single) {
            if let Some(at) = self.active.remove(&key) {
                let status = match current_finish {
                    Some(FinishReason::Complete) => TurnStatus::Complete,
                    Some(FinishReason::Length) => TurnStatus::Length,
                    Some(FinishReason::Cancelled) => TurnStatus::Cancelled,
                    Some(FinishReason::Error) => TurnStatus::Failed,
                    _ => TurnStatus::Incomplete,
                };
                events.push(TurnEvent::Completed(at.finalize(status)));
                self.metrics.counter(Metric::TurnsCompleted).inc();
            }
        }

        events
    }

    /// Advance the virtual clock using an external time signal (a capture
    /// heartbeat forwarded through the pipeline) and run sweep. This lets
    /// idle turns finalize even when no new call arrives on this shard.
    ///
    /// Monotonic: `virtual_now_us` only moves forward. Sweep runs at most
    /// once per `sweep_interval_us`, so calling this frequently is safe.
    ///
    /// `ts` is in the same unit as `LlmCall.request_time` (Unix-epoch µs).
    pub fn advance_time(&mut self, ts: i64) -> Vec<TurnEvent> {
        self.virtual_now_us = self.virtual_now_us.max(ts);
        self.sweep()
    }

    /// Called by the harness periodically (or on packet time advance).
    /// Finalizes any turn whose last_activity is older than idle_timeout.
    pub fn sweep(&mut self) -> Vec<TurnEvent> {
        if self.virtual_now_us - self.last_sweep_us < self.config.sweep_interval_us {
            return Vec::new();
        }
        self.last_sweep_us = self.virtual_now_us;
        let cutoff = self.virtual_now_us - self.config.idle_timeout_us;
        let expired_keys: Vec<TurnKey> = self
            .active
            .iter()
            .filter(|(_, t)| t.last_activity_us < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let mut events = Vec::with_capacity(expired_keys.len());
        for key in expired_keys {
            if let Some(turn) = self.active.remove(&key) {
                let status = match turn.last_finish_reason {
                    Some(FinishReason::Complete) => TurnStatus::Complete,
                    Some(FinishReason::Length) => TurnStatus::Length,
                    Some(FinishReason::Cancelled) => TurnStatus::Cancelled,
                    Some(FinishReason::Error) => TurnStatus::Failed,
                    _ => TurnStatus::Incomplete,
                };
                events.push(TurnEvent::Completed(turn.finalize(status)));
                self.metrics.counter(Metric::TurnsTimedOut).inc();
                self.metrics.counter(Metric::TurnsCompleted).inc();
            }
        }
        events
    }

    /// Called at EOF or shutdown. Finalizes all active turns as Incomplete.
    pub fn flush_all(&mut self) -> Vec<TurnEvent> {
        let keys: Vec<TurnKey> = self
            .active
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut events = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(turn) = self.active.remove(&key) {
                events.push(TurnEvent::Completed(turn.finalize(TurnStatus::Incomplete)));
                self.metrics.counter(Metric::TurnsCompleted).inc();
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use ts_llm::model::{ApiType, LlmCall, ProviderFormat};
    use ts_llm::profiles;

    fn test_metrics() -> MetricsWorker {
        use ts_common::internal_metrics::MetricsSystem;
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker("test", &[
            Metric::TurnCallsIngested,
            Metric::TurnCallsAuxiliary,
            Metric::TurnsCompleted,
            Metric::TurnsTimedOut,
        ]);
        let _svc = sys.start();
        w
    }

    fn identity_for(
        call: &ts_llm::model::LlmCall,
        profile_name: &'static str,
    ) -> ts_llm::model::CallIdentity {
        let reg = profiles::build_default_registry();
        let profile = reg.find_by_name(profile_name).expect("known profile");
        let ids = profile.extract_ids(call).expect("profile extract_ids");
        ts_llm::model::CallIdentity {
            profile_name,
            client_kind: profile_name.to_string(),
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        }
    }

    fn codex_call(session: &str, turn: &str, body_input_type: &str, finish: FinishReason) -> LlmCall {
        let meta = format!(r#"{{"session_id":"{session}","turn_id":"{turn}"}}"#);
        let body = match body_input_type {
            "message" => r#"{"input":[{"type":"message","role":"user","content":"hi"}]}"#.to_string(),
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

    #[test]
    fn codex_same_turn_id_accumulates() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c1 = codex_call("s1", "t1", "message", FinishReason::ToolUse); // user-start
        let c2 = codex_call("s1", "t1", "function_call_output", FinishReason::ToolUse); // continuation
        let id1 = identity_for(&c1, "codex-cli");
        let id2 = identity_for(&c2, "codex-cli");
        let e1 = t.ingest(&c1, &id1);
        let e2 = t.ingest(&c2, &id2);
        // Both added to same turn
        assert_eq!(t.active_count(), 1);
        assert!(e1.iter().any(|e| matches!(e, TurnEvent::Started { .. })));
        assert!(e2.iter().any(|e| matches!(e, TurnEvent::CallAdded { .. })));
    }

    #[test]
    fn codex_new_turn_id_opens_new_turn_and_closes_old() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c1 = codex_call("s1", "t1", "function_call_output", FinishReason::ToolUse);
        let c2 = codex_call("s1", "t2", "message", FinishReason::ToolUse);
        let id1 = identity_for(&c1, "codex-cli");
        let id2 = identity_for(&c2, "codex-cli");
        t.ingest(&c1, &id1);
        let events = t.ingest(&c2, &id2);
        // Old turn (t1) must be emitted as Completed; new turn (t2) Started.
        assert!(events
            .iter()
            .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.turn_id == "t1")));
        assert!(events
            .iter()
            .any(|e| matches!(e, TurnEvent::Started { key, .. } if key.turn_id == "t2")));
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn tracker_starts_empty() {
        let t = TurnTracker::new(Arc::new(ProfileRegistry::new()), TrackerConfig::default(), test_metrics());
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn flush_all_on_empty_tracker_returns_no_events() {
        let mut t = TurnTracker::new(Arc::new(ProfileRegistry::new()), TrackerConfig::default(), test_metrics());
        assert!(t.flush_all().is_empty());
    }

    #[test]
    fn sweep_respects_sweep_interval() {
        let mut t = TurnTracker::new(
            Arc::new(ProfileRegistry::new()),
            TrackerConfig {
                idle_timeout_us: 0,
                sweep_interval_us: 5_000_000,
            },
            test_metrics(),
        );
        t.virtual_now_us = 1_000_000; // 1s < 5s interval
        assert!(t.sweep().is_empty());
        t.virtual_now_us = 6_000_000; // 6s > 5s interval
                                      // no active turns to sweep, still no events — but last_sweep_us updates
        let _ = t.sweep();
    }

    fn anthropic_call(
        session: &str,
        request_time_us: i64,
        body_last_content_type: &str,
        finish: FinishReason,
    ) -> LlmCall {
        let body = match body_last_content_type {
            "text" => r#"{"messages":[{"role":"user","content":[{"type":"text","text":"go"}]}]}"#,
            "tool_result" => r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#,
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

    #[test]
    fn anthropic_captures_user_input_and_final_answer() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());

        let mut c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        c1.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>ignore</system-reminder>plan the refactor"}]}]}"#.into(),
        );

        let mut c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete);
        c2.response_body =
            Some(r#"{"content":[{"type":"text","text":"Done. Here is the result."}]}"#.into());

        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        t.ingest(&c1, &id1);
        let events = t.ingest(&c2, &id2);
        let turn = events
            .iter()
            .find_map(|e| match e {
                TurnEvent::Completed(tr) => Some(tr),
                _ => None,
            })
            .expect("turn should be completed");

        assert_eq!(turn.user_input_preview.as_deref(), Some("plan the refactor"));
        assert_eq!(turn.user_call_id.as_deref(), Some(&c1.id[..]));
        assert_eq!(
            turn.final_answer_preview.as_deref(),
            Some("Done. Here is the result.")
        );
        assert_eq!(turn.final_call_id.as_deref(), Some("c-2000000"));
    }

    #[test]
    fn final_answer_preview_is_truncated() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let long_text = "x".repeat(1000);
        let body = format!(r#"{{"content":[{{"type":"text","text":"{long_text}"}}]}}"#);

        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let mut c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete);
        c2.response_body = Some(body);

        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        t.ingest(&c1, &id1);
        let events = t.ingest(&c2, &id2);
        let turn = events
            .iter()
            .find_map(|e| match e {
                TurnEvent::Completed(tr) => Some(tr),
                _ => None,
            })
            .expect("turn should be completed");

        let preview = turn.final_answer_preview.as_deref().unwrap();
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), FINAL_ANSWER_PREVIEW_CHARS + 1);
    }

    #[test]
    fn user_input_preview_is_truncated() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let long_text = "u".repeat(1000);
        let body = format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"{long_text}"}}]}}]}}"#
        );
        let mut c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        c1.request_body = Some(body);
        let c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete);
        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        t.ingest(&c1, &id1);
        let events = t.ingest(&c2, &id2);
        let turn = events
            .iter()
            .find_map(|e| match e {
                TurnEvent::Completed(tr) => Some(tr),
                _ => None,
            })
            .expect("turn should be completed");
        let preview = turn.user_input_preview.as_deref().unwrap();
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), USER_INPUT_PREVIEW_CHARS + 1);
        assert_eq!(turn.user_call_id.as_deref(), Some(&c1.id[..]));
    }

    #[test]
    fn subagent_complete_does_not_close_parent_turn() {
        // Main-agent call (c1) → sub-agent call (c2, tools without "Agent")
        // that finishes with Complete → main-agent continuation (c3) with
        // a tool_result body. Before the fix, c2's Complete would be stored
        // in ActiveTurn.last_finish_reason, causing c3 to see `terminal=true`
        // and prematurely split the turn. After the fix, sub-agent terminal
        // signals are dropped on the floor and the parent turn stays open.
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());

        let mut c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        c1.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"go"}]}],"tools":[{"name":"Agent"},{"name":"Bash"}]}"#.into(),
        );
        let mut c2 = anthropic_call("S", 2_000_000, "text", FinishReason::Complete);
        c2.request_body = Some(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],"tools":[{"name":"Read"},{"name":"Grep"}]}"#.into(),
        );
        let c3 = anthropic_call("S", 3_000_000, "tool_result", FinishReason::ToolUse);

        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        let id3 = identity_for(&c3, "claude-cli");
        t.ingest(&c1, &id1);
        let e2 = t.ingest(&c2, &id2);
        let e3 = t.ingest(&c3, &id3);
        // Sub-agent finished with Complete, but parent turn must stay open.
        assert!(
            !e2.iter().any(|e| matches!(e, TurnEvent::Completed(_))),
            "sub-agent Complete must not close parent"
        );
        assert!(
            !e3.iter().any(|e| matches!(e, TurnEvent::Completed(_))),
            "main-agent continuation must not see terminal state from sub-agent"
        );
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn subagent_assistant_text_does_not_leak_to_parent_final_answer() {
        // Sub-agent responses carry assistant text that belongs to the
        // sub-agent's own conclusion; it must not overwrite the parent
        // turn's final_answer_preview / final_call_id.
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());

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

        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        let id3 = identity_for(&c3, "claude-cli");
        t.ingest(&c1, &id1);
        t.ingest(&c2, &id2);
        let events = t.ingest(&c3, &id3);
        let turn = events
            .iter()
            .find_map(|e| match e {
                TurnEvent::Completed(tr) => Some(tr),
                _ => None,
            })
            .expect("turn closes on main-agent Complete");
        assert_eq!(turn.final_answer_preview.as_deref(), Some("final answer"));
        assert_eq!(turn.final_call_id.as_deref(), Some(&c3.id[..]));
        assert!(turn.subagents_used.iter().any(|s| s == "task"));
    }

    #[test]
    fn anthropic_tool_use_keeps_turn_open() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::ToolUse);
        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        t.ingest(&c1, &id1);
        t.ingest(&c2, &id2);
        // Both in the same generated turn; no Completed yet.
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn anthropic_end_turn_closes_and_next_user_message_opens_new_turn() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let c2 = anthropic_call("S", 2_000_000, "tool_result", FinishReason::Complete); // end_turn
        let c3 = anthropic_call("S", 3_000_000, "text", FinishReason::Complete);

        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        let id3 = identity_for(&c3, "claude-cli");
        t.ingest(&c1, &id1);
        let e2 = t.ingest(&c2, &id2);
        assert!(e2
            .iter()
            .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.status == TurnStatus::Complete)));
        assert_eq!(t.active_count(), 0);

        let e3 = t.ingest(&c3, &id3);
        assert!(e3.iter().any(|e| matches!(e, TurnEvent::Started { .. })));
        // c3 is user-initiated + Complete → confident single-call turn,
        // closes immediately.
        assert!(e3
            .iter()
            .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.status == TurnStatus::Complete)));
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn anthropic_new_user_message_without_end_turn_closes_old_as_incomplete() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse); // no end_turn
        let c2 = anthropic_call("S", 2_000_000, "text", FinishReason::Complete); // new user start

        let id1 = identity_for(&c1, "claude-cli");
        let id2 = identity_for(&c2, "claude-cli");
        t.ingest(&c1, &id1);
        let e2 = t.ingest(&c2, &id2);
        assert!(e2
            .iter()
            .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.status == TurnStatus::Incomplete)));
        assert!(e2.iter().any(|e| matches!(e, TurnEvent::Started { .. })));
    }

    #[test]
    fn sweep_finalizes_idle_turn_as_incomplete() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(
            Arc::new(reg),
            TrackerConfig {
                idle_timeout_us: 500_000_000, // 500s
                sweep_interval_us: 1_000_000, // 1s
            },
            test_metrics(),
        );
        let c1 = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let id1 = identity_for(&c1, "claude-cli");
        t.ingest(&c1, &id1);
        assert_eq!(t.active_count(), 1);

        // Advance virtual time past idle_timeout by ingesting a call from a DIFFERENT session.
        let c2 = anthropic_call("OTHER", 600_000_000, "text", FinishReason::Complete);
        let id2 = identity_for(&c2, "claude-cli");
        t.ingest(&c2, &id2);
        // Now virtual_now = 600s; original turn is 599s idle → sweep finalizes it.
        let events = t.sweep();
        assert!(events.iter().any(|e| matches!(
            e, TurnEvent::Completed(tr) if tr.session_id == "S" && tr.status == TurnStatus::Incomplete
        )));
    }

    #[test]
    fn flush_all_finalizes_every_active_turn() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::ToolUse);
        let id = identity_for(&c, "claude-cli");
        t.ingest(&c, &id);
        assert_eq!(t.active_count(), 1);
        let events = t.flush_all();
        assert_eq!(events.len(), 1);
        assert_eq!(t.active_count(), 0);
        assert!(
            matches!(&events[0], TurnEvent::Completed(tr) if tr.status == TurnStatus::Incomplete)
        );
    }

    #[test]
    fn ingest_with_identity_skips_registry_find() {
        use ts_llm::model::CallIdentity;

        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());

        let call = anthropic_call("S", 1_000_000, "text", FinishReason::Complete);
        let identity = CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".into(),
            session_id: "S".into(),
            turn_id_hint: None,
        };

        let events = t.ingest(&call, &identity);
        assert!(events
            .iter()
            .any(|e| matches!(e, TurnEvent::Started { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, TurnEvent::CallAdded { .. })));
    }

    #[test]
    fn ingest_populates_call_ids_into_finalized_turn() {
        use ts_llm::model::CallIdentity;

        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());

        let c1 = codex_call("s1", "t1", "hello", FinishReason::ToolUse);
        let c2 = codex_call("s1", "t1", "world", FinishReason::ToolUse);
        let id1 = CallIdentity {
            profile_name: "codex-cli",
            client_kind: "codex-cli".into(),
            session_id: "s1".into(),
            turn_id_hint: Some("t1".into()),
        };
        let id2 = id1.clone();
        t.ingest(&c1, &id1);
        t.ingest(&c2, &id2);
        let finalized: Vec<_> = t
            .flush_all()
            .into_iter()
            .filter_map(|e| match e {
                TurnEvent::Completed(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].call_ids, vec![c1.id.clone(), c2.id.clone()]);
    }

    #[test]
    fn auxiliary_call_is_skipped_entirely() {
        use ts_llm::model::CallIdentity;

        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());

        // tools:[] → ClaudeCliProfile::is_auxiliary returns true.
        let mut call = anthropic_call("S", 1_000_000, "text", FinishReason::Complete);
        call.request_body = Some(
            r#"{"messages":[{"role":"user","content":"generate title"}],"tools":[]}"#.into(),
        );
        let identity = CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".into(),
            session_id: "S".into(),
            turn_id_hint: None,
        };
        let events = t.ingest(&call, &identity);
        assert!(events.is_empty(), "aux call should emit no events");
        assert_eq!(t.active_count(), 0, "aux call must not open a turn");
    }

    #[test]
    fn ingest_with_identity_honors_explicit_turn_id() {
        use ts_llm::model::CallIdentity;

        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());

        let call = codex_call("s1", "t1", "message", FinishReason::ToolUse);
        let identity = CallIdentity {
            profile_name: "codex-cli",
            client_kind: "codex-cli".into(),
            session_id: "s1".into(),
            turn_id_hint: Some("t1".into()),
        };
        let events = t.ingest(&call, &identity);
        assert!(events
            .iter()
            .any(|e| matches!(e, TurnEvent::Started { .. })));
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn single_call_complete_closes_immediately() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::Complete);
        let id = identity_for(&c, "claude-cli");
        let events = t.ingest(&c, &id);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.status == TurnStatus::Complete)),
            "single user-initiated Complete call should close turn immediately"
        );
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn single_call_length_closes_immediately() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::Length);
        let id = identity_for(&c, "claude-cli");
        let events = t.ingest(&c, &id);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.status == TurnStatus::Length)),
            "single user-initiated Length call should close turn immediately"
        );
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn single_call_error_stays_open() {
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c = anthropic_call("S", 1_000_000, "text", FinishReason::Error);
        let id = identity_for(&c, "claude-cli");
        let events = t.ingest(&c, &id);
        assert!(
            !events.iter().any(|e| matches!(e, TurnEvent::Completed(_))),
            "single Error call should NOT close turn (client may retry)"
        );
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn codex_complete_does_not_close_turn_immediately() {
        // Responses-API: `Complete` means "API call succeeded," not "turn done."
        // Without a terminal-output signal, turn stays open.
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let c = codex_call("s1", "t1", "message", FinishReason::Complete);
        let id = identity_for(&c, "codex-cli");
        let events = t.ingest(&c, &id);
        assert!(
            !events.iter().any(|e| matches!(e, TurnEvent::Completed(_))),
            "codex Complete without terminal-output predicate should NOT close turn"
        );
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn codex_terminal_output_closes_turn_immediately() {
        // Plan B: when the response carries only a final assistant message
        // (no function_call items), the explicit-path close fires without
        // waiting for the next turn_id or the idle-timeout sweep.
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let mut c = codex_call("s1", "t1", "message", FinishReason::Complete);
        c.response_body = Some(
            r#"{"output":[
                {"type":"reasoning","summary":[]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"done."}]}
            ]}"#
            .to_string(),
        );
        let id = identity_for(&c, "codex-cli");
        let events = t.ingest(&c, &id);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.status == TurnStatus::Complete)),
            "terminal-output codex call should close turn immediately as Complete"
        );
        assert_eq!(t.active_count(), 0);
    }

    #[test]
    fn codex_pending_function_call_keeps_turn_open() {
        // Output contains a function_call ⇒ codex will issue another request;
        // turn must stay open even though finish_reason is Complete.
        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(Arc::new(reg), TrackerConfig::default(), test_metrics());
        let mut c = codex_call("s1", "t1", "message", FinishReason::Complete);
        c.response_body = Some(
            r#"{"output":[
                {"type":"reasoning","summary":[]},
                {"type":"function_call","name":"shell","call_id":"c1","arguments":"{}"}
            ]}"#
            .to_string(),
        );
        let id = identity_for(&c, "codex-cli");
        let events = t.ingest(&c, &id);
        assert!(
            !events.iter().any(|e| matches!(e, TurnEvent::Completed(_))),
            "pending function_call should NOT close turn"
        );
        assert_eq!(t.active_count(), 1);
    }

    #[test]
    fn codex_sweep_infers_complete_status() {
        // After idle timeout, sweep should infer TurnStatus from last_finish_reason.
        let reg = profiles::build_default_registry();
        let cfg = TrackerConfig {
            idle_timeout_us: 1_000,
            sweep_interval_us: 1_000,
        };
        let mut t = TurnTracker::new(Arc::new(reg), cfg, test_metrics());
        let c = codex_call("s1", "t1", "message", FinishReason::Complete);
        let id = identity_for(&c, "codex-cli");
        t.ingest(&c, &id);
        assert_eq!(t.active_count(), 1);

        // Advance time past idle timeout (last_activity = complete_time = 2_000_000)
        let swept = t.advance_time(c.complete_time.unwrap() + 2_000);
        assert!(
            swept
                .iter()
                .any(|e| matches!(e, TurnEvent::Completed(tr) if tr.status == TurnStatus::Complete)),
            "sweep should infer Complete from last_finish_reason"
        );
        assert_eq!(t.active_count(), 0);
    }
}
