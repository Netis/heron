//! Per-profile primitive extraction over captured request-body fixtures.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use ts_llm::agent_primitives::{AgentPrimitives, SystemPromptMarkers};
use ts_llm::profile::AgentProfile;

fn load(name: &str) -> Value {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/agent_primitives");
    path.push(name);
    let body = fs::read_to_string(&path).expect("fixture must exist");
    serde_json::from_str(&body).expect("fixture must be valid JSON")
}

fn run<P: AgentProfile>(profile: &P, fixture: &str) -> AgentPrimitives {
    let req = load(fixture);
    let llm_call = ts_llm::test_support::empty_llm_call();
    let ctx = ts_llm::profile::CallCtx::new(&llm_call, Some(&req), None);
    profile.extract_primitives(&ctx)
}

#[test]
fn claude_cli_tool_use_extracts_tool_name_and_agent_marker() {
    let p = run(
        &ts_llm::agents::ClaudeCliProfile,
        "claude_cli_tool_use.json",
    );
    assert_eq!(p.tool_call_count, 1);
    assert_eq!(p.tool_names, vec!["Read".to_string()]);
    assert!(p
        .system_prompt_markers
        .contains(SystemPromptMarkers::AGENT_LOOP));
}

#[test]
fn claude_cli_subagent_sets_dispatches_to_subagent() {
    let p = run(
        &ts_llm::agents::ClaudeCliProfile,
        "claude_cli_subagent.json",
    );
    assert!(p.dispatches_to_subagent);
    assert!(p.tool_names.iter().any(|n| n == "Task"));
}

#[test]
fn codex_cli_function_calls_counted() {
    let p = run(
        &ts_llm::agents::CodexCliProfile,
        "codex_cli_function_calls.json",
    );
    assert_eq!(p.tool_call_count, 2);
    assert_eq!(p.tool_names, vec!["Read".to_string(), "Edit".to_string()]);
}

#[test]
fn opencode_mcp_tool_visible() {
    let p = run(&ts_llm::agents::OpencodeProfile, "opencode_mcp.json");
    assert!(p.tool_names.iter().any(|n| n.starts_with("mcp__")));
}

#[test]
fn hermes_bash_tool_visible() {
    let p = run(&ts_llm::agents::HermesProfile, "hermes_bash.json");
    assert!(p.tool_names.iter().any(|n| n == "bash"));
}

#[test]
fn generic_unknown_tool_visible() {
    let p = run(&ts_llm::agents::GenericProfile, "generic_unknown_tool.json");
    assert_eq!(p.tool_names, vec!["mystery_tool".to_string()]);
}
