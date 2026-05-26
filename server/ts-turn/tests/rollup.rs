//! Verify the turn-finalize rollup combines per-call agent fields into the
//! correct aggregate fields on `AgentTurn`.

use ts_common::agent::{AgentTopology, ToolSurface};
use ts_llm::agent_classifier::SuspiciousSignal;
use ts_turn::test_support::{feed_session_and_finalize, make_call, AgentCall};

#[test]
fn rollup_collects_distinct_surfaces() {
    let calls = vec![
        make_call("c1", Some(ToolSurface::FunctionCall), 2, &["Read", "Edit"]),
        make_call("c2", Some(ToolSurface::Mcp), 1, &["mcp__svc__do"]),
        make_call("c3", Some(ToolSurface::FunctionCall), 1, &["Bash"]),
    ];
    let turn = feed_session_and_finalize("session-1", &calls);
    assert!(turn.tool_surfaces.contains(&ToolSurface::FunctionCall));
    assert!(turn.tool_surfaces.contains(&ToolSurface::Mcp));
    assert_eq!(turn.tool_surfaces.len(), 2);
}

#[test]
fn rollup_sums_tool_call_total() {
    let calls = vec![
        make_call("c1", Some(ToolSurface::FunctionCall), 2, &["Read", "Edit"]),
        make_call(
            "c2",
            Some(ToolSurface::FunctionCall),
            3,
            &["Bash", "Write", "Grep"],
        ),
    ];
    let turn = feed_session_and_finalize("session-2", &calls);
    assert_eq!(turn.tool_call_total, 5);
}

#[test]
fn rollup_topology_precedes_orchestrator() {
    let calls = vec![
        make_call_with_topology("c1", AgentTopology::SingleAgent),
        make_call_with_topology("c2", AgentTopology::Orchestrator),
        make_call_with_topology("c3", AgentTopology::SubAgent),
    ];
    let turn = feed_session_and_finalize("session-3", &calls);
    assert_eq!(turn.agent_topology, Some(AgentTopology::Orchestrator));
}

#[test]
fn rollup_dedupes_suspicious_skills() {
    let mut c1 = make_call("c1", Some(ToolSurface::Unknown), 1, &["mystery"]);
    c1.agent.suspicious_signals = vec![SuspiciousSignal::UnknownToolName {
        name: "mystery".to_string(),
    }];
    let mut c2 = make_call("c2", Some(ToolSurface::Unknown), 1, &["mystery"]);
    c2.agent.suspicious_signals = vec![SuspiciousSignal::UnknownToolName {
        name: "mystery".to_string(),
    }];
    let turn = feed_session_and_finalize("session-4", &[c1, c2]);
    assert_eq!(turn.suspicious_skills.len(), 1);
    assert_eq!(turn.suspicious_skills[0].tool_name, "mystery");
}

fn make_call_with_topology(id: &str, topology: AgentTopology) -> AgentCall {
    let mut c = make_call(id, Some(ToolSurface::FunctionCall), 1, &["Read"]);
    c.agent.agent_topology = Some(topology);
    c
}
