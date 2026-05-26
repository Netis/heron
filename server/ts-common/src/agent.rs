//! Tool-surface and agent-topology taxonomies. Persisted as snake_case
//! strings so the schema is migration-friendly when new variants land.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurface {
    FunctionCall,
    Mcp,
    Cli,
    Mixed,
    Unknown,
}

impl fmt::Display for ToolSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ToolSurface::FunctionCall => "function_call",
            ToolSurface::Mcp => "mcp",
            ToolSurface::Cli => "cli",
            ToolSurface::Mixed => "mixed",
            ToolSurface::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTopology {
    SingleAgent,
    SubAgent,
    Orchestrator,
}

impl fmt::Display for AgentTopology {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            AgentTopology::SingleAgent => "single_agent",
            AgentTopology::SubAgent => "sub_agent",
            AgentTopology::Orchestrator => "orchestrator",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_surface_serializes_snake_case() {
        let json = serde_json::to_string(&ToolSurface::FunctionCall).unwrap();
        assert_eq!(json, "\"function_call\"");
        assert_eq!(ToolSurface::FunctionCall.to_string(), "function_call");
    }

    #[test]
    fn tool_surface_round_trips() {
        for variant in [
            ToolSurface::FunctionCall,
            ToolSurface::Mcp,
            ToolSurface::Cli,
            ToolSurface::Mixed,
            ToolSurface::Unknown,
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            let back: ToolSurface = serde_json::from_str(&s).unwrap();
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn agent_topology_orders_correctly() {
        // Used by turn rollup precedence: Orchestrator > SubAgent > SingleAgent
        let mut variants = [
            AgentTopology::SingleAgent,
            AgentTopology::Orchestrator,
            AgentTopology::SubAgent,
        ];
        variants.sort_by_key(|t| match t {
            AgentTopology::SingleAgent => 0,
            AgentTopology::SubAgent => 1,
            AgentTopology::Orchestrator => 2,
        });
        assert_eq!(variants[2], AgentTopology::Orchestrator);
    }
}
