//! Turn grouping: aggregates `LlmCall` into `AgentTurn` per agent session.
//!
//! Header-explicit only — calls without a matching `AgentProfile` do not
//! participate in turn grouping.

pub mod model;
pub mod proxy_pair;
pub mod stage;
pub mod test_support;
pub mod tracker;

pub use model::{new_active_turn_registry, ActiveTurnRegistry, AgentTurn, TurnKey, TurnStatus};
pub use proxy_pair::{
    candidate_from_turn, group_all, GroupMember, PairCandidate, ProxyGroup, ProxyRole,
    MAX_REQ_TIME_GAP_US, MIRROR_TIME_TOLERANCE_US,
};
pub use stage::spawn_turn_stage;
pub use tracker::{TurnEvent, TurnTracker};

/// One suspicious tool flagged during rollup. Serialized into
/// `agent_turns.suspicious_skills_json` as a JSON array of objects.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SuspiciousSkillRollup {
    pub tool_name: String,
    pub reason: String,
}
