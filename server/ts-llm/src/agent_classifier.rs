//! Profile-agnostic agent classifier. Consumes `AgentPrimitives` (filled by
//! extractors) and derives the persisted fields on `LlmCall.agent`.

use crate::agent_primitives::AgentPrimitives;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use ts_common::agent::{AgentTopology, ToolSurface};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClassifierConfig {
    pub cli_tool_allowlist: BTreeSet<String>,
    pub orchestrator_tool_names: BTreeSet<String>,
    pub mcp_tool_prefixes: Vec<String>,
    pub known_tool_registry: BTreeSet<String>,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            cli_tool_allowlist: ["bash", "shell", "exec", "run_command", "ShellExec"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            orchestrator_tool_names: ["Task", "dispatch_agent"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            mcp_tool_prefixes: vec!["mcp__".to_string()],
            known_tool_registry: [
                "Read",
                "Edit",
                "Write",
                "Grep",
                "Bash",
                "Task",
                "WebFetch",
                "WebSearch",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuspiciousSignal {
    UnknownToolName { name: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassifiedAgent {
    pub is_agent_request: bool,
    pub tool_surface: Option<ToolSurface>,
    pub agent_topology: Option<AgentTopology>,
    pub suspicious_signals: Vec<SuspiciousSignal>,
}

pub fn classify(_primitives: &AgentPrimitives, _cfg: &ClassifierConfig) -> ClassifiedAgent {
    // Implemented in Task 4.
    ClassifiedAgent::default()
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn default_config_has_expected_seeds() {
        let cfg = ClassifierConfig::default();
        assert!(cfg.cli_tool_allowlist.contains("bash"));
        assert!(cfg.orchestrator_tool_names.contains("Task"));
        assert_eq!(cfg.mcp_tool_prefixes, vec!["mcp__".to_string()]);
        assert!(cfg.known_tool_registry.contains("Read"));
    }

    #[test]
    fn config_round_trips_via_serde() {
        let cfg = ClassifierConfig::default();
        let s = toml::to_string(&cfg).unwrap();
        let back: ClassifierConfig = toml::from_str(&s).unwrap();
        assert_eq!(cfg.cli_tool_allowlist, back.cli_tool_allowlist);
        assert_eq!(cfg.orchestrator_tool_names, back.orchestrator_tool_names);
    }
}
