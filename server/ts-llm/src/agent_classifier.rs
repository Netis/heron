//! Profile-agnostic agent classifier. Consumes `AgentPrimitives` (filled by
//! extractors) and derives the persisted fields on `LlmCall.agent`.

use crate::agent_primitives::{AgentPrimitives, SystemPromptMarkers};
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

pub fn classify(primitives: &AgentPrimitives, cfg: &ClassifierConfig) -> ClassifiedAgent {
    let has_tool_calls = primitives.tool_call_count > 0;
    let has_agent_markers = primitives.system_prompt_markers.intersects(
        SystemPromptMarkers::AGENT_LOOP
            | SystemPromptMarkers::REACT_SKELETON
            | SystemPromptMarkers::TOOL_USE_INSTRUCTION,
    );
    let is_agent_request =
        has_tool_calls || primitives.subagent_marker.is_some() || has_agent_markers;

    let (tool_surface, suspicious_signals) = if !has_tool_calls {
        (None, Vec::new())
    } else {
        let mut has_mcp = primitives
            .system_prompt_markers
            .contains(SystemPromptMarkers::MCP_SERVER);
        let mut has_cli = false;
        let mut has_function_call = false;
        let mut suspicious = Vec::new();

        for name in &primitives.tool_names {
            let is_mcp_tool = cfg.mcp_tool_prefixes.iter().any(|p| name.starts_with(p));
            let is_cli_tool = cfg.cli_tool_allowlist.contains(name);
            let is_known = cfg.known_tool_registry.contains(name);

            if is_mcp_tool {
                has_mcp = true;
            } else if is_cli_tool {
                has_cli = true;
            } else if is_known {
                has_function_call = true;
            } else {
                suspicious.push(SuspiciousSignal::UnknownToolName { name: name.clone() });
            }
        }

        let active = [has_mcp, has_cli, has_function_call]
            .iter()
            .filter(|x| **x)
            .count();
        let surface = match active {
            0 => Some(ToolSurface::Unknown),
            1 if has_mcp => Some(ToolSurface::Mcp),
            1 if has_cli => Some(ToolSurface::Cli),
            1 => Some(ToolSurface::FunctionCall),
            _ => Some(ToolSurface::Mixed),
        };

        (surface, suspicious)
    };

    let agent_topology = if !is_agent_request {
        None
    } else if primitives.subagent_marker.is_some() {
        Some(AgentTopology::SubAgent)
    } else if primitives.dispatches_to_subagent {
        Some(AgentTopology::Orchestrator)
    } else {
        Some(AgentTopology::SingleAgent)
    };

    ClassifiedAgent {
        is_agent_request,
        tool_surface,
        agent_topology,
        suspicious_signals,
    }
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

#[cfg(test)]
mod classify_tests {
    use super::*;
    use crate::agent_primitives::SystemPromptMarkers;

    fn cfg() -> ClassifierConfig {
        ClassifierConfig::default()
    }

    fn prims_with_tools(names: &[&str]) -> AgentPrimitives {
        AgentPrimitives {
            tool_call_count: names.len() as u32,
            tool_names: names.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    // is_agent_request rules ---------------------------------------------

    #[test]
    fn is_agent_when_tool_calls_present() {
        let c = classify(&prims_with_tools(&["Read"]), &cfg());
        assert!(c.is_agent_request);
    }

    #[test]
    fn is_agent_when_subagent_marker_present() {
        let p = AgentPrimitives {
            subagent_marker: Some("Task".to_string()),
            ..Default::default()
        };
        let c = classify(&p, &cfg());
        assert!(c.is_agent_request);
    }

    #[test]
    fn is_agent_when_system_prompt_has_agent_loop_marker() {
        let p = AgentPrimitives {
            has_system_prompt: true,
            system_prompt_markers: SystemPromptMarkers::AGENT_LOOP,
            ..Default::default()
        };
        let c = classify(&p, &cfg());
        assert!(c.is_agent_request);
    }

    #[test]
    fn is_not_agent_when_no_signals_present() {
        let c = classify(&AgentPrimitives::default(), &cfg());
        assert!(!c.is_agent_request);
        assert_eq!(c.tool_surface, None);
        assert_eq!(c.agent_topology, None);
    }

    // tool_surface rules -------------------------------------------------

    #[test]
    fn surface_function_call_for_native_tool_name() {
        let c = classify(&prims_with_tools(&["Read", "Edit"]), &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::FunctionCall));
    }

    #[test]
    fn surface_mcp_for_mcp_prefix() {
        let c = classify(&prims_with_tools(&["mcp__github__list_issues"]), &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::Mcp));
    }

    #[test]
    fn surface_mcp_for_system_prompt_mcp_marker() {
        let p = AgentPrimitives {
            tool_call_count: 1,
            tool_names: vec!["custom_tool".to_string()],
            has_system_prompt: true,
            system_prompt_markers: SystemPromptMarkers::MCP_SERVER,
            ..Default::default()
        };
        let c = classify(&p, &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::Mcp));
    }

    #[test]
    fn surface_cli_for_bash_tool() {
        let c = classify(&prims_with_tools(&["bash"]), &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::Cli));
    }

    #[test]
    fn surface_mixed_for_two_surfaces() {
        let c = classify(&prims_with_tools(&["bash", "mcp__svc__do"]), &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::Mixed));
    }

    #[test]
    fn surface_unknown_for_unregistered_tool() {
        let c = classify(&prims_with_tools(&["mystery_tool"]), &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::Unknown));
    }

    // agent_topology rules -----------------------------------------------

    #[test]
    fn topology_sub_agent_when_marker_present() {
        let p = AgentPrimitives {
            tool_call_count: 1,
            tool_names: vec!["Read".to_string()],
            subagent_marker: Some("sub".to_string()),
            ..Default::default()
        };
        let c = classify(&p, &cfg());
        assert_eq!(c.agent_topology, Some(AgentTopology::SubAgent));
    }

    #[test]
    fn topology_orchestrator_when_dispatches_to_subagent() {
        let p = AgentPrimitives {
            tool_call_count: 1,
            tool_names: vec!["Task".to_string()],
            dispatches_to_subagent: true,
            ..Default::default()
        };
        let c = classify(&p, &cfg());
        assert_eq!(c.agent_topology, Some(AgentTopology::Orchestrator));
    }

    #[test]
    fn topology_single_agent_default() {
        let c = classify(&prims_with_tools(&["Read"]), &cfg());
        assert_eq!(c.agent_topology, Some(AgentTopology::SingleAgent));
    }

    // suspicious -----------------------------------------------------------

    #[test]
    fn suspicious_flags_unknown_tool_name() {
        let c = classify(&prims_with_tools(&["mystery_tool"]), &cfg());
        assert_eq!(
            c.suspicious_signals,
            vec![SuspiciousSignal::UnknownToolName {
                name: "mystery_tool".to_string()
            }]
        );
    }

    #[test]
    fn suspicious_empty_for_known_tools() {
        let c = classify(&prims_with_tools(&["Read", "Edit"]), &cfg());
        assert!(c.suspicious_signals.is_empty());
    }
}
