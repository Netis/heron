//! Passive llmproxy pair detection — pairs duplicate `AgentTurn` records
//! that represent the same logical LLM call observed at two different
//! network vantage points.
//!
//! Two scenarios produce duplicate turns in TokenScope:
//!
//! 1. **Real proxy hops** — e.g. an external client → haproxy_glm5 container
//!    → sglang container. Both legs cross interfaces TokenScope captures, so
//!    each becomes its own `LlmCall` and then its own `AgentTurn` (the
//!    agent-profile session hash already groups them under one
//!    `session_id`, but the tracker still partitions them into separate
//!    turns because each carries an independent user-turn-start). The
//!    proxy_in leg strictly contains the proxy_out leg in event time.
//!
//! 2. **Multi-interface double-capture** — libpcap on `any` interface
//!    captures the *same* packet once on `br0` and once on `docker0`
//!    (different NAT-rewritten views of the same bytes). The two turns
//!    have near-identical timestamps (~ms apart) but distinct
//!    `(client_ip, server_ip, server_port)` 5-tuples.
//!
//! Both end up confusing the user with redundant rows in
//! `/api/agent-turns`. We pair them via content + tight time window +
//! differing 5-tuple, then the API filters one out by default.
//!
//! ### Why not topology (A.server_ip == B.client_ip)
//!
//! Docker bridges SNAT outbound traffic from a container's IP to the
//! bridge gateway IP (172.17.0.1 instead of the originating container's
//! 172.17.0.9). The proxy host's *listen* IP and its *outbound* IP differ
//! on captured packets, so the obvious topological signal is unreliable.
//! Content + timing is the rule that survives.
//!
//! ### Why we pair at the turn level (not call level)
//!
//! In live data the two legs land in *different* `AgentTurn` records
//! (each leg looks like a turn-start to its tracker shard), so the user
//! sees duplicates on the *list* page — not inside a single turn. Pairing
//! has to happen on turns to actually fold the list.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::AgentTurn;

/// Role of a turn inside a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyRole {
    /// Outer leg — the client-facing entry into the proxy. Strict event-
    /// time nesting: `proxy_in.start ≤ proxy_out.start` and
    /// `proxy_in.end ≥ proxy_out.end`. This is the leg the user sees by
    /// default.
    ProxyIn,
    /// Inner leg — the proxy's outbound call to the real upstream. Hidden
    /// from the default list view.
    ProxyOut,
    /// Same packet captured twice on different interfaces. Times overlap
    /// within `MIRROR_TIME_TOLERANCE_US` on both ends. Primary is the
    /// representative (kept by default); secondary is hidden.
    MirrorPrimary,
    /// See `MirrorPrimary` — the duplicate copy to hide.
    MirrorSecondary,
}

impl ProxyRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyRole::ProxyIn => "proxy_in",
            ProxyRole::ProxyOut => "proxy_out",
            ProxyRole::MirrorPrimary => "mirror_primary",
            ProxyRole::MirrorSecondary => "mirror_secondary",
        }
    }

    /// Whether the API list view should hide this role by default.
    pub fn hidden_by_default(self) -> bool {
        matches!(self, ProxyRole::ProxyOut | ProxyRole::MirrorSecondary)
    }
}

/// Pairing annotation attached to a turn's `metadata.proxy` JSON field.
/// Both members of a pair carry the same `pair_id`; each one's
/// `peer_turn_id` points at the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyPair {
    pub role: ProxyRole,
    pub pair_id: String,
    pub peer_turn_id: String,
}

/// Maximum gap between the two legs' request_times for them to be
/// considered the same logical call. Live data shows real proxy hops at
/// 2ms and mirror duplicates at <1ms; 100ms gives generous headroom for
/// slower proxies (LiteLLM forwarding to a remote upstream).
pub const MAX_REQ_TIME_GAP_US: i64 = 100_000;

/// For mirror classification: both start_time and end_time must agree
/// within this tolerance. Same-packet double-capture on different
/// interfaces sees identical kernel timestamps modulo libpcap dispatch
/// jitter (<100us in practice). Real proxy hops — even the cheapest
/// in-container ones — introduce at least 1ms of forwarding overhead, so
/// 500us cleanly separates the two cases. Don't widen this without
/// re-checking that the verified haproxy_glm5 turn pair from wuneng
/// (start_gap 2ms, end_gap 1ms) still classifies as strict-nesting.
pub const MIRROR_TIME_TOLERANCE_US: i64 = 500;

/// Light fingerprint of an `AgentTurn` carrying just the fields the
/// pairing rule needs. Pulled from DB via a narrow projection so the
/// sweeper doesn't materialize every column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairCandidate {
    pub turn_id: String,
    pub session_id: String,
    pub agent_kind: String,
    pub wire_api: String,
    pub start_time_us: i64,
    pub end_time_us: i64,
    pub call_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub final_finish_reason: Option<String>,
    pub primary_model: Option<String>,
    /// Stable string form of `(client_ip, server_ip)` used purely to ensure
    /// the two candidates observed the call from different vantage points.
    /// Server port is intentionally excluded — different proxy hops differ
    /// only on server port in some topologies, and including it doesn't
    /// add discriminating power.
    pub network_view: String,
}

/// Outcome of pairing two candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairAssignment {
    pub pair_id: String,
    pub primary: PairMember,
    pub secondary: PairMember,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairMember {
    pub turn_id: String,
    pub role: ProxyRole,
}

impl PairAssignment {
    pub fn new(primary_id: &str, primary_role: ProxyRole, secondary_id: &str, secondary_role: ProxyRole) -> Self {
        Self {
            pair_id: format!("pair-{}", Uuid::now_v7()),
            primary: PairMember {
                turn_id: primary_id.to_string(),
                role: primary_role,
            },
            secondary: PairMember {
                turn_id: secondary_id.to_string(),
                role: secondary_role,
            },
        }
    }

    /// JSON value to merge into both turns' `metadata` (one for each
    /// member). The caller writes the appropriate variant to each turn.
    pub fn metadata_for(&self, turn_id: &str) -> Option<serde_json::Value> {
        let (me, peer) = if turn_id == self.primary.turn_id {
            (&self.primary, &self.secondary)
        } else if turn_id == self.secondary.turn_id {
            (&self.secondary, &self.primary)
        } else {
            return None;
        };
        Some(serde_json::json!({
            "proxy": {
                "role": me.role.as_str(),
                "pair_id": self.pair_id,
                "peer_turn_id": peer.turn_id,
            }
        }))
    }
}

/// Determine whether two candidates are a pair, and if so the role each
/// plays. Returns `None` if they don't match the pairing rule.
///
/// Rule (all must hold):
/// * Same `session_id` — already linked by content-hashing agent profiles.
/// * Same `agent_kind`, `wire_api`, `call_count`, token counts, finish
///   reason, primary model — content equivalence.
/// * Differing `network_view` — the whole point: same call, two vantages.
/// * `|a.start_time_us - b.start_time_us| ≤ MAX_REQ_TIME_GAP_US`.
///
/// Role:
/// * Mirror (`MirrorPrimary`/`MirrorSecondary`) when both start and end
///   times agree within `MIRROR_TIME_TOLERANCE_US`. Primary = the one with
///   the lexicographically smaller `turn_id` (deterministic, stable
///   across re-sweeps).
/// * Otherwise, strict nesting: the leg whose start is earlier *and* end
///   is later is `ProxyIn`; the other is `ProxyOut`. If neither nests
///   strictly, the pair is ambiguous and we return `None` rather than
///   guess.
pub fn classify_pair(a: &PairCandidate, b: &PairCandidate) -> Option<PairAssignment> {
    if a.turn_id == b.turn_id {
        return None;
    }
    if a.session_id != b.session_id
        || a.agent_kind != b.agent_kind
        || a.wire_api != b.wire_api
        || a.call_count != b.call_count
        || a.total_input_tokens != b.total_input_tokens
        || a.total_output_tokens != b.total_output_tokens
        || a.final_finish_reason != b.final_finish_reason
        || a.primary_model != b.primary_model
    {
        return None;
    }
    if a.network_view == b.network_view {
        return None;
    }
    let dt = (a.start_time_us - b.start_time_us).abs();
    if dt > MAX_REQ_TIME_GAP_US {
        return None;
    }

    let start_gap = (a.start_time_us - b.start_time_us).abs();
    let end_gap = (a.end_time_us - b.end_time_us).abs();

    if start_gap <= MIRROR_TIME_TOLERANCE_US && end_gap <= MIRROR_TIME_TOLERANCE_US {
        // Mirror: same packet, different interfaces. Primary = lexicographically
        // smaller turn_id for determinism.
        let (primary, secondary) = if a.turn_id < b.turn_id { (a, b) } else { (b, a) };
        return Some(PairAssignment::new(
            &primary.turn_id,
            ProxyRole::MirrorPrimary,
            &secondary.turn_id,
            ProxyRole::MirrorSecondary,
        ));
    }

    // Strict nesting: outer contains inner.
    let (outer, inner) = if a.start_time_us <= b.start_time_us && a.end_time_us >= b.end_time_us {
        (a, b)
    } else if b.start_time_us <= a.start_time_us && b.end_time_us >= a.end_time_us {
        (b, a)
    } else {
        return None;
    };
    Some(PairAssignment::new(
        &outer.turn_id,
        ProxyRole::ProxyIn,
        &inner.turn_id,
        ProxyRole::ProxyOut,
    ))
}

/// Pair every candidate in `set` exactly once. Uses a session-keyed
/// bucket so the per-session search is small. Within a session, candidates
/// are matched greedily by closest start_time gap (ties broken by
/// secondary nesting check); each candidate participates in at most one
/// pair. Candidates that don't find a peer are simply omitted from the
/// returned list (no `direct` marker — absence of `metadata.proxy` IS
/// the direct case).
pub fn pair_all(set: &[PairCandidate]) -> Vec<PairAssignment> {
    let mut by_session: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in set.iter().enumerate() {
        by_session.entry(c.session_id.as_str()).or_default().push(i);
    }
    let mut taken = vec![false; set.len()];
    let mut out = Vec::new();

    for ids in by_session.values() {
        for &i in ids {
            if taken[i] {
                continue;
            }
            // Find the closest unused peer that classifies as a pair.
            let mut best: Option<(usize, i64, PairAssignment)> = None;
            for &j in ids {
                if i == j || taken[j] {
                    continue;
                }
                if let Some(p) = classify_pair(&set[i], &set[j]) {
                    let dt = (set[i].start_time_us - set[j].start_time_us).abs();
                    match best {
                        Some((_, prev_dt, _)) if prev_dt <= dt => {}
                        _ => best = Some((j, dt, p)),
                    }
                }
            }
            if let Some((j, _, p)) = best {
                taken[i] = true;
                taken[j] = true;
                out.push(p);
            }
        }
    }
    out
}

/// Build a `PairCandidate` from an `AgentTurn`, used by callers that have
/// the full turn in memory (e.g. unit tests). Production callers will
/// build candidates directly from a DB projection.
pub fn candidate_from_turn(t: &AgentTurn) -> PairCandidate {
    PairCandidate {
        turn_id: t.turn_id.clone(),
        session_id: t.session_id.clone(),
        agent_kind: t.agent_kind.clone(),
        wire_api: t.wire_api.clone(),
        start_time_us: t.start_time_us,
        end_time_us: t.end_time_us,
        call_count: t.call_count,
        total_input_tokens: t.total_input_tokens,
        total_output_tokens: t.total_output_tokens,
        final_finish_reason: t.final_finish_reason.clone(),
        primary_model: t.models_used.first().cloned(),
        network_view: format!("{}->{}", t.client_ip, t.server_ip),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(
        turn_id: &str,
        session: &str,
        start_us: i64,
        end_us: i64,
        net_view: &str,
    ) -> PairCandidate {
        PairCandidate {
            turn_id: turn_id.into(),
            session_id: session.into(),
            agent_kind: "openclaw".into(),
            wire_api: "openai-chat".into(),
            start_time_us: start_us,
            end_time_us: end_us,
            call_count: 1,
            total_input_tokens: 11345,
            total_output_tokens: 128,
            final_finish_reason: Some("stop".into()),
            primary_model: Some("GLM-5.1".into()),
            network_view: net_view.into(),
        }
    }

    #[test]
    fn proxy_hop_strict_nesting() {
        // Mirrors the verified haproxy_glm5 pair from wuneng:
        // outer turn (proxy_in) starts 2us earlier and ends 1us later than
        // the inner upstream call.
        let outer = mk("d3d6", "S", 348_294_000, 350_588_000, "172.16.103.100->172.17.0.9");
        let inner = mk("d3ec", "S", 348_296_000, 350_587_000, "172.17.0.1->172.17.0.4");
        let p = classify_pair(&outer, &inner).expect("nested pair");
        assert_eq!(p.primary.turn_id, "d3d6");
        assert_eq!(p.primary.role, ProxyRole::ProxyIn);
        assert_eq!(p.secondary.turn_id, "d3ec");
        assert_eq!(p.secondary.role, ProxyRole::ProxyOut);
        // Order-independent: swapping arguments yields the same assignment.
        let p2 = classify_pair(&inner, &outer).expect("nested pair (rev)");
        assert_eq!(p2.primary.turn_id, "d3d6");
        assert_eq!(p2.primary.role, ProxyRole::ProxyIn);
    }

    #[test]
    fn mirror_when_times_agree_on_both_ends() {
        // Same packet captured on br0 and docker0: <1ms apart on both ends.
        let a = mk("aaaa", "S", 100_000, 200_000, "C->host_ip");
        let b = mk("bbbb", "S", 100_500, 200_500, "C->container_ip");
        let p = classify_pair(&a, &b).expect("mirror pair");
        assert_eq!(p.primary.role, ProxyRole::MirrorPrimary);
        assert_eq!(p.secondary.role, ProxyRole::MirrorSecondary);
        // Primary is the lexicographically smaller turn_id (deterministic).
        assert_eq!(p.primary.turn_id, "aaaa");
    }

    #[test]
    fn does_not_pair_across_sessions() {
        let a = mk("a", "session_one", 100, 200, "v1");
        let b = mk("b", "session_two", 100, 200, "v2");
        assert!(classify_pair(&a, &b).is_none());
    }

    #[test]
    fn does_not_pair_same_network_view() {
        // Two unrelated calls from the same client/server pair within
        // 100ms — coincidence, not a proxy hop. We must NOT pair them.
        let a = mk("a", "S", 100, 200, "C->S");
        let b = mk("b", "S", 150, 250, "C->S");
        assert!(classify_pair(&a, &b).is_none());
    }

    #[test]
    fn does_not_pair_when_time_gap_exceeds_window() {
        let a = mk("a", "S", 0, 1_000_000, "v1");
        let b = mk("b", "S", 200_000, 1_200_000, "v2");
        assert!(classify_pair(&a, &b).is_none());
    }

    #[test]
    fn does_not_pair_when_tokens_differ() {
        let a = mk("a", "S", 0, 1_000, "v1");
        let mut b = mk("b", "S", 50, 1_050, "v2");
        b.total_input_tokens = 11344;
        assert!(classify_pair(&a, &b).is_none());
    }

    #[test]
    fn ambiguous_non_nesting_rejected() {
        // a starts earlier but ends earlier too — neither contains the
        // other. Could be two concurrent independent calls.
        let a = mk("a", "S", 0, 500_000, "v1");
        let b = mk("b", "S", 50_000, 800_000, "v2");
        assert!(classify_pair(&a, &b).is_none());
    }

    #[test]
    fn metadata_for_emits_role_and_peer() {
        let p = PairAssignment::new("t1", ProxyRole::ProxyIn, "t2", ProxyRole::ProxyOut);
        let meta_t1 = p.metadata_for("t1").unwrap();
        assert_eq!(meta_t1["proxy"]["role"], "proxy_in");
        assert_eq!(meta_t1["proxy"]["peer_turn_id"], "t2");
        let meta_t2 = p.metadata_for("t2").unwrap();
        assert_eq!(meta_t2["proxy"]["role"], "proxy_out");
        assert_eq!(meta_t2["proxy"]["peer_turn_id"], "t1");
        assert_eq!(p.metadata_for("unknown"), None);
    }

    #[test]
    fn pair_all_handles_two_pairs_in_one_session() {
        // Two distinct proxy hops in the same session — both should be
        // paired, neither should bleed across.
        let cands = vec![
            mk("a1", "S", 1_000, 5_000, "front->host"),
            mk("a2", "S", 1_500, 4_500, "bridge->upstream"),
            mk("b1", "S", 10_000, 15_000, "front->host"),
            mk("b2", "S", 10_500, 14_500, "bridge->upstream"),
        ];
        let pairs = pair_all(&cands);
        assert_eq!(pairs.len(), 2);
        let mut paired_ids: Vec<String> = pairs
            .iter()
            .flat_map(|p| [p.primary.turn_id.clone(), p.secondary.turn_id.clone()])
            .collect();
        paired_ids.sort();
        assert_eq!(paired_ids, vec!["a1", "a2", "b1", "b2"]);
    }

    #[test]
    fn pair_all_leaves_unmatched_alone() {
        // One solo turn, no peer.
        let cands = vec![mk("solo", "S", 0, 1000, "view")];
        assert!(pair_all(&cands).is_empty());
    }
}
