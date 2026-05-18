//! Passive llmproxy pair detection — folds 2+ `AgentTurn` records that
//! represent the same logical LLM call observed at different network
//! vantage points.
//!
//! Three (or more) scenarios produce duplicate turns in TokenScope:
//!
//! 1. **Real proxy hops** — e.g. an external client → haproxy_glm5 container
//!    → sglang container. Both legs cross interfaces TokenScope captures
//!    so each becomes its own `AgentTurn`. The proxy_in leg strictly
//!    contains the proxy_out leg in event time.
//!
//! 2. **Multi-interface double-capture** — libpcap on `any` interface
//!    captures the *same* packet once on `br0` (host-IP view) and once
//!    on `docker0` (container-IP view). Near-identical timestamps,
//!    distinct `(client_ip, server_ip)` 5-tuples.
//!
//! 3. **Both at once (the haproxy_glm5 case)** — three legs per call:
//!    a host-IP view of the inbound + a docker-IP view of the same
//!    inbound (mirror pair) + the proxy's outbound hop to the real
//!    upstream (strictly nested inside the mirror pair). All three
//!    represent the same logical request and should fold into one row.
//!
//! ### Why N-member groups instead of pairs
//!
//! The original 2-member pair algorithm could not collapse the
//! haproxy_glm5 case — given {A, B, C} where (A, B) are mirrors and C
//! is nested inside both, the greedy "closest peer" rule paired the
//! 0ms mirror first and left C with no available peer. We model the
//! result as a **group** of arbitrary size; every member shares the
//! same `group_id` and points at every other peer via `peer_turn_ids`.
//!
//! ### Why not topology (A.server_ip == B.client_ip)
//!
//! Docker bridges SNAT outbound traffic from a container's IP to the
//! bridge gateway IP (172.17.0.1 instead of the originating container's
//! 172.17.0.9). The proxy host's *listen* IP and its *outbound* IP differ
//! on captured packets, so the obvious topological signal is unreliable.
//! Content + timing is the rule that survives.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::AgentTurn;

/// Role of a turn inside its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyRole {
    /// Outer leg — the client-facing entry into the proxy. Strict event-
    /// time nesting: `proxy_in.start ≤ other.start` and
    /// `proxy_in.end ≥ other.end`. Visible in the default list view.
    /// When a group contains both nested members and mirrors, the
    /// canonical leg takes `ProxyIn` (the more informative role).
    ProxyIn,
    /// Inner leg — the proxy's outbound call to the real upstream. Hidden
    /// from the default list view.
    ProxyOut,
    /// Same packet captured twice on different interfaces. Times overlap
    /// within `MIRROR_TIME_TOLERANCE_US` on both ends with the canonical.
    /// Used when no nested member exists; otherwise the canonical
    /// upgrades to `ProxyIn` and mirror members downgrade to
    /// `MirrorSecondary`.
    MirrorPrimary,
    /// Hidden mirror copy of the canonical.
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
    /// `proxy_in` and `mirror_primary` stay visible (one row per
    /// logical call); the rest fold under them.
    pub fn hidden_by_default(self) -> bool {
        matches!(self, ProxyRole::ProxyOut | ProxyRole::MirrorSecondary)
    }
}

/// Maximum gap between any two members' start_times for them to be
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
    /// Stable string form of `(client_ip, server_ip)` used to ensure
    /// group members observed the call from different vantage points.
    /// Server port is intentionally excluded — different proxy hops differ
    /// only on server port in some topologies, and including it doesn't
    /// add discriminating power.
    pub network_view: String,
}

/// A cluster of 2+ turns that represent the same logical LLM call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyGroup {
    pub group_id: String,
    pub members: Vec<GroupMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    pub turn_id: String,
    pub role: ProxyRole,
}

impl ProxyGroup {
    fn new(members: Vec<GroupMember>) -> Self {
        Self {
            group_id: format!("group-{}", Uuid::now_v7()),
            members,
        }
    }

    /// JSON value to merge into `turn_id`'s `metadata` field. Includes
    /// `peer_turn_ids` (every other member's turn_id, sorted lex) and
    /// the legacy `peer_turn_id` (the first peer in that list, for
    /// pre-multi-leg API consumers).
    pub fn metadata_for(&self, turn_id: &str) -> Option<serde_json::Value> {
        let me = self.members.iter().find(|m| m.turn_id == turn_id)?;
        let mut peers: Vec<String> = self
            .members
            .iter()
            .filter(|m| m.turn_id != turn_id)
            .map(|m| m.turn_id.clone())
            .collect();
        peers.sort();
        let first_peer = peers.first().cloned();
        Some(serde_json::json!({
            "proxy": {
                "role": me.role.as_str(),
                // We keep the field name `pair_id` instead of `group_id`
                // so already-shipped consumers and stored metadata stay
                // valid. It's an opaque identifier — the rename would
                // only be cosmetic.
                "pair_id": self.group_id,
                "peer_turn_ids": peers,
                // Legacy single-peer pointer kept for backward
                // compatibility — older API clients only know about the
                // 2-member case. Returns the lex-first peer when the
                // group has multiple peers.
                "peer_turn_id": first_peer,
            }
        }))
    }
}

/// Content fingerprint (everything that must agree across all members
/// of a group) — used to bucket candidates before time-clustering.
fn content_fingerprint(
    c: &PairCandidate,
) -> (
    &str,
    &str,
    &str,
    u32,
    u64,
    u64,
    Option<&str>,
    Option<&str>,
) {
    (
        c.session_id.as_str(),
        c.agent_kind.as_str(),
        c.wire_api.as_str(),
        c.call_count,
        c.total_input_tokens,
        c.total_output_tokens,
        c.final_finish_reason.as_deref(),
        c.primary_model.as_deref(),
    )
}

/// Within a content-fingerprint-equal bucket, walk in start_time order
/// and grow a cluster as long as the next turn falls within
/// `MAX_REQ_TIME_GAP_US` of the *latest* member's start_time. A new
/// cluster opens as soon as the gap is exceeded.
fn time_clusters(mut idxs: Vec<usize>, set: &[PairCandidate]) -> Vec<Vec<usize>> {
    idxs.sort_by_key(|&i| set[i].start_time_us);
    let mut out: Vec<Vec<usize>> = Vec::new();
    for i in idxs {
        let pushed = if let Some(cluster) = out.last() {
            let last_start = set[*cluster.last().unwrap()].start_time_us;
            set[i].start_time_us - last_start <= MAX_REQ_TIME_GAP_US
        } else {
            false
        };
        if pushed {
            out.last_mut().unwrap().push(i);
        } else {
            out.push(vec![i]);
        }
    }
    out
}

/// Pick the canonical member of a cluster: the leg with the widest time
/// span (smallest start_time, largest end_time). When two legs tie on
/// span (true mirrors), the lexicographically smallest `turn_id` wins
/// — purely deterministic so re-sweeps yield the same result.
fn pick_canonical(cluster: &[usize], set: &[PairCandidate]) -> usize {
    cluster
        .iter()
        .copied()
        .min_by(|&a, &b| {
            let ca = &set[a];
            let cb = &set[b];
            // Widest span first. start_time ASC, then end_time DESC,
            // then turn_id ASC as tiebreaker.
            ca.start_time_us
                .cmp(&cb.start_time_us)
                .then_with(|| cb.end_time_us.cmp(&ca.end_time_us))
                .then_with(|| ca.turn_id.cmp(&cb.turn_id))
        })
        .expect("non-empty cluster")
}

/// Assign per-member roles relative to `canonical_idx`. Returns the
/// canonical's role too — `ProxyIn` if any non-canonical was classified
/// `ProxyOut`, else `MirrorPrimary`. Members whose time relationship
/// with the canonical is neither mirror-overlap nor strict-nesting are
/// returned as `None` — the caller drops them from the group (they
/// stay unpaired).
fn assign_roles(
    cluster: &[usize],
    canonical_idx: usize,
    set: &[PairCandidate],
) -> Vec<(usize, Option<ProxyRole>)> {
    let canon = &set[canonical_idx];
    cluster
        .iter()
        .copied()
        .map(|i| {
            if i == canonical_idx {
                return (i, None); // Filled in below by caller
            }
            let c = &set[i];
            if c.network_view == canon.network_view {
                // Same vantage on both — not a duplicate, drop from group.
                return (i, None);
            }
            let start_gap = (c.start_time_us - canon.start_time_us).abs();
            let end_gap = (c.end_time_us - canon.end_time_us).abs();
            if start_gap <= MIRROR_TIME_TOLERANCE_US && end_gap <= MIRROR_TIME_TOLERANCE_US {
                (i, Some(ProxyRole::MirrorSecondary))
            } else if canon.start_time_us <= c.start_time_us && canon.end_time_us >= c.end_time_us
            {
                (i, Some(ProxyRole::ProxyOut))
            } else {
                (i, None) // ambiguous time relationship
            }
        })
        .collect()
}

/// Group every candidate in `set` into maximal duplicate clusters.
/// A candidate that doesn't fit any cluster (no peer found) is simply
/// omitted from the returned list — absence of `metadata.proxy` is the
/// signal for "direct, non-duplicate turn".
pub fn group_all(set: &[PairCandidate]) -> Vec<ProxyGroup> {
    let mut by_fp: HashMap<(&str, &str, &str, u32, u64, u64, Option<&str>, Option<&str>), Vec<usize>> =
        HashMap::new();
    for (i, c) in set.iter().enumerate() {
        by_fp.entry(content_fingerprint(c)).or_default().push(i);
    }

    let mut groups = Vec::new();
    for (_, ids) in by_fp {
        if ids.len() < 2 {
            continue;
        }
        for cluster in time_clusters(ids, set) {
            if cluster.len() < 2 {
                continue;
            }
            let canonical_idx = pick_canonical(&cluster, set);
            let assignments = assign_roles(&cluster, canonical_idx, set);
            // Drop ambiguous members.
            let valid_non_canon: Vec<(usize, ProxyRole)> = assignments
                .iter()
                .filter_map(|(i, r)| {
                    if *i == canonical_idx {
                        None
                    } else {
                        r.map(|role| (*i, role))
                    }
                })
                .collect();
            if valid_non_canon.is_empty() {
                continue;
            }
            // Canonical's role: ProxyIn if any peer is a real hop,
            // else MirrorPrimary.
            let canonical_role = if valid_non_canon
                .iter()
                .any(|(_, r)| *r == ProxyRole::ProxyOut)
            {
                ProxyRole::ProxyIn
            } else {
                ProxyRole::MirrorPrimary
            };
            let mut members: Vec<GroupMember> = Vec::with_capacity(valid_non_canon.len() + 1);
            members.push(GroupMember {
                turn_id: set[canonical_idx].turn_id.clone(),
                role: canonical_role,
            });
            for (i, role) in valid_non_canon {
                members.push(GroupMember {
                    turn_id: set[i].turn_id.clone(),
                    role,
                });
            }
            groups.push(ProxyGroup::new(members));
        }
    }
    groups
}

/// Build a `PairCandidate` from an `AgentTurn`, used by callers that have
/// the full turn in memory (e.g. unit tests). Production callers build
/// candidates directly from a DB projection.
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

    fn role_of(g: &ProxyGroup, turn_id: &str) -> Option<ProxyRole> {
        g.members.iter().find(|m| m.turn_id == turn_id).map(|m| m.role)
    }

    #[test]
    fn proxy_hop_strict_nesting_yields_two_member_group() {
        // Mirrors the verified haproxy_glm5 pair from wuneng: outer
        // proxy_in starts 2us earlier and ends 1us later than the
        // inner upstream call.
        let outer = mk("d3d6", "S", 348_294_000, 350_588_000, "172.16.103.100->172.17.0.9");
        let inner = mk("d3ec", "S", 348_296_000, 350_587_000, "172.17.0.1->172.17.0.4");
        let groups = group_all(&[outer, inner]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.members.len(), 2);
        assert_eq!(role_of(g, "d3d6"), Some(ProxyRole::ProxyIn));
        assert_eq!(role_of(g, "d3ec"), Some(ProxyRole::ProxyOut));
    }

    #[test]
    fn mirror_pair_collapses_when_times_agree_on_both_ends() {
        // Same packet captured on br0 and docker0 — <500us apart on
        // both ends.
        let a = mk("aaaa", "S", 100_000, 200_000, "C->host_ip");
        let b = mk("bbbb", "S", 100_200, 200_200, "C->container_ip");
        let groups = group_all(&[a, b]);
        assert_eq!(groups.len(), 1);
        assert_eq!(role_of(&groups[0], "aaaa"), Some(ProxyRole::MirrorPrimary));
        assert_eq!(role_of(&groups[0], "bbbb"), Some(ProxyRole::MirrorSecondary));
    }

    #[test]
    fn haproxy_three_leg_collapses_into_single_group() {
        // The real-world scenario the user is asking for. Three legs
        // per logical call:
        //   A — host-IP view of inbound (br0)
        //   B — docker-IP view of the same inbound (mirror of A)
        //   C — haproxy's outbound to upstream container (real hop,
        //       nested inside the mirror pair)
        let a = mk("a_br0", "S", 1_000_000, 3_000_000, "172.16.103.100->172.16.103.81");
        let b = mk("b_dock0", "S", 1_000_000, 3_000_000, "172.16.103.100->172.17.0.9");
        let c = mk("c_hop", "S", 1_002_000, 2_999_000, "172.17.0.1->172.17.0.4");
        let groups = group_all(&[a, b, c]);
        assert_eq!(groups.len(), 1, "all three must fold into one group");
        let g = &groups[0];
        assert_eq!(g.members.len(), 3);
        // Canonical has the wider time span. a_br0 and b_dock0 tie on
        // span; lex-smallest turn_id wins → "a_br0".
        let canon_role = role_of(g, "a_br0").unwrap();
        assert_eq!(canon_role, ProxyRole::ProxyIn);
        // b_dock0 is the mirror of canonical
        assert_eq!(role_of(g, "b_dock0"), Some(ProxyRole::MirrorSecondary));
        // c_hop is the real upstream hop
        assert_eq!(role_of(g, "c_hop"), Some(ProxyRole::ProxyOut));
        // metadata_for builds the right view for each member
        let meta_canon = g.metadata_for("a_br0").unwrap();
        let peers: Vec<&str> = meta_canon["proxy"]["peer_turn_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(peers, vec!["b_dock0", "c_hop"]);
        // Legacy peer_turn_id field still set to the first peer
        assert_eq!(meta_canon["proxy"]["peer_turn_id"], "b_dock0");
    }

    #[test]
    fn does_not_pair_across_sessions() {
        let a = mk("a", "session_one", 100, 200, "v1");
        let b = mk("b", "session_two", 100, 200, "v2");
        assert!(group_all(&[a, b]).is_empty());
    }

    #[test]
    fn does_not_pair_same_network_view() {
        // Two unrelated calls from the same client/server pair within
        // 100ms — coincidence, not a duplicate.
        let a = mk("a", "S", 100, 200, "C->S");
        let b = mk("b", "S", 150, 250, "C->S");
        assert!(group_all(&[a, b]).is_empty());
    }

    #[test]
    fn does_not_pair_when_time_gap_exceeds_window() {
        let a = mk("a", "S", 0, 1_000_000, "v1");
        // Start gap 200ms — well past MAX_REQ_TIME_GAP_US.
        let b = mk("b", "S", 200_000, 1_200_000, "v2");
        assert!(group_all(&[a, b]).is_empty());
    }

    #[test]
    fn does_not_pair_when_tokens_differ() {
        let a = mk("a", "S", 0, 1_000, "v1");
        let mut b = mk("b", "S", 50, 1_050, "v2");
        b.total_input_tokens = 11344;
        assert!(group_all(&[a, b]).is_empty());
    }

    #[test]
    fn ambiguous_member_dropped_keeps_remaining_group() {
        // A + B is a valid pair. C has the same content fingerprint
        // and falls in the time window but its time relationship with
        // the canonical isn't nesting or mirror (overlapping but
        // neither contains the other). C is dropped; A+B still pair.
        let a = mk("a", "S", 0, 2_000_000, "v1");
        let b = mk("b", "S", 0, 2_000_000, "v2"); // mirror of a
        let c = mk("c", "S", 1_000_000, 3_000_000, "v3"); // overlap but no nesting w/ canon
        let groups = group_all(&[a, b, c]);
        // Group has only A and B; C is ambiguous (start in canonical's
        // span but end outside it) and gets dropped.
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.members.len(), 2);
        let ids: Vec<&str> = g.members.iter().map(|m| m.turn_id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(!ids.contains(&"c"));
    }

    #[test]
    fn pair_all_handles_two_independent_groups_in_one_session() {
        // Two distinct logical calls in the same session — both should
        // be paired independently. Distinct tokens makes them distinct
        // fingerprints so they cluster apart even without the time
        // window check.
        let mut a1 = mk("a1", "S", 1_000, 5_000, "front->host");
        let mut a2 = mk("a2", "S", 1_500, 4_500, "bridge->upstream");
        let mut b1 = mk("b1", "S", 10_000, 15_000, "front->host");
        let mut b2 = mk("b2", "S", 10_500, 14_500, "bridge->upstream");
        // Different token counts so the two pairs don't merge.
        a1.total_input_tokens = 100;
        a2.total_input_tokens = 100;
        b1.total_input_tokens = 200;
        b2.total_input_tokens = 200;
        let groups = group_all(&[a1, a2, b1, b2]);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn lone_turns_omitted() {
        let cands = vec![mk("solo", "S", 0, 1000, "view")];
        assert!(group_all(&cands).is_empty());
    }

    #[test]
    fn metadata_for_unknown_turn_id_returns_none() {
        let g = ProxyGroup::new(vec![
            GroupMember { turn_id: "t1".into(), role: ProxyRole::ProxyIn },
            GroupMember { turn_id: "t2".into(), role: ProxyRole::ProxyOut },
        ]);
        assert!(g.metadata_for("unrelated").is_none());
    }
}
