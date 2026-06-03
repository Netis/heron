//! Profile-agnostic agent classifier. Consumes `AgentPrimitives` (filled by
//! extractors) and derives the persisted fields on `LlmCall.agent`.

use crate::agent_primitives::{AgentPrimitives, SystemPromptMarkers};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use h_common::agent::{AgentTopology, ToolSurface};

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
            // Current Claude Code + Codex CLI built-in tool inventory. The old
            // 8-entry seed flagged everything else (Glob, TodoWrite, the Task*
            // family, …) as `UnknownToolName`, which surfaced legit agent
            // traffic as "suspicious" and inflated AgentClassifierUnknownCount
            // (#85). Grouped by source CLI; extend here as the CLIs add tools.
            // A `[classifier] known_tool_registry = [...]` TOML override
            // replaces this list wholesale (see `from_toml`).
            known_tool_registry: [
                // Claude Code — file & notebook ops
                "Read", "Write", "Edit", "MultiEdit", "NotebookEdit", "NotebookRead",
                // Claude Code — search
                "Grep", "Glob",
                // Claude Code — shell / background-process management
                "Bash", "BashOutput", "KillShell", "KillBash",
                // Claude Code — web
                "WebFetch", "WebSearch",
                // Claude Code — planning & meta
                "TodoWrite", "ExitPlanMode", "SlashCommand", "Skill", "AskUserQuestion",
                // Claude Code — subagent / task orchestration
                "Task", "Agent", "TaskCreate", "TaskGet", "TaskList", "TaskOutput",
                "TaskStop", "TaskUpdate",
                // Codex CLI
                "apply_patch", "update_plan", "view_image",
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
        // Retained for the return shape + stored `suspicious_skills` column, but
        // no longer populated: we don't flag unrecognized tool names (see loop).
        let suspicious: Vec<SuspiciousSignal> = Vec::new();

        for name in &primitives.tool_names {
            let is_mcp_tool = cfg.mcp_tool_prefixes.iter().any(|p| name.starts_with(p));
            let is_cli_tool = cfg.cli_tool_allowlist.contains(name);

            if is_mcp_tool {
                has_mcp = true;
            } else if is_cli_tool {
                has_cli = true;
            } else {
                // Any other named tool is a function-call tool. We deliberately
                // do NOT gate this on an allow-list: every agent (Claude Code,
                // Codex, openclaw, …) ships dozens of differently-named,
                // differently-cased tools (`read` vs `Read`, `web_fetch`,
                // `memory_get`, `sessions_spawn`, `mattermost_send`, …). An
                // unrecognized name means "a toolset we don't enumerate", NOT
                // "suspicious" — flagging it tagged ~14% of real agent calls as
                // suspicious (pure noise that buried any genuine signal) and was
                // an un-winnable allow-list treadmill. See #85: expanding the
                // registry only ever covered Claude Code; this removes the
                // premise instead. `known_tool_registry` is retained in config
                // for back-compat but no longer changes classification.
                has_function_call = true;
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

impl ClassifierConfig {
    /// Build from TOML config — empty Vec fields fall back to the seed values
    /// of `ClassifierConfig::default()`. This keeps `default.toml` minimal
    /// while still allowing operators to fully override any list.
    pub fn from_toml(toml_cfg: &h_common::config::ClassifierConfigToml) -> Self {
        let seeds = Self::default();
        Self {
            cli_tool_allowlist: if toml_cfg.cli_tool_allowlist.is_empty() {
                seeds.cli_tool_allowlist
            } else {
                toml_cfg.cli_tool_allowlist.iter().cloned().collect()
            },
            orchestrator_tool_names: if toml_cfg.orchestrator_tool_names.is_empty() {
                seeds.orchestrator_tool_names
            } else {
                toml_cfg.orchestrator_tool_names.iter().cloned().collect()
            },
            mcp_tool_prefixes: if toml_cfg.mcp_tool_prefixes.is_empty() {
                seeds.mcp_tool_prefixes
            } else {
                toml_cfg.mcp_tool_prefixes.clone()
            },
            known_tool_registry: if toml_cfg.known_tool_registry.is_empty() {
                seeds.known_tool_registry
            } else {
                toml_cfg.known_tool_registry.iter().cloned().collect()
            },
        }
    }
}

#[cfg(test)]
mod toml_tests {
    use super::*;
    use h_common::config::ClassifierConfigToml;

    #[test]
    fn empty_toml_falls_back_to_defaults() {
        let toml_cfg = ClassifierConfigToml::default();
        let cfg = ClassifierConfig::from_toml(&toml_cfg);
        assert!(cfg.cli_tool_allowlist.contains("bash"));
        assert!(cfg.orchestrator_tool_names.contains("Task"));
        assert_eq!(cfg.mcp_tool_prefixes, vec!["mcp__".to_string()]);
        assert!(cfg.known_tool_registry.contains("Read"));
    }

    #[test]
    fn populated_toml_overrides() {
        let mut toml_cfg = ClassifierConfigToml::default();
        toml_cfg.cli_tool_allowlist = vec!["zsh".to_string()];
        let cfg = ClassifierConfig::from_toml(&toml_cfg);
        assert!(cfg.cli_tool_allowlist.contains("zsh"));
        assert!(!cfg.cli_tool_allowlist.contains("bash"));
    }

    #[test]
    fn shipped_default_toml_inherits_full_registry() {
        // Regression for #93: the shipped default.toml must leave
        // known_tool_registry EMPTY so `from_toml` inherits the built-in list.
        // A hardcoded copy there silently overrode the default ("non-empty TOML
        // wins") and shipped a stale 8-tool set to every default deployment.
        // Path is resolved from the crate manifest so it's CWD-independent.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config/default.toml");
        let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        #[derive(serde::Deserialize)]
        struct Wrap {
            #[serde(default)]
            agent_classifier: ClassifierConfigToml,
        }
        let wrap: Wrap = toml::from_str(&raw).expect("default.toml should parse");
        let cfg = ClassifierConfig::from_toml(&wrap.agent_classifier);
        assert!(
            cfg.known_tool_registry.contains("Glob")
                && cfg.known_tool_registry.contains("TaskOutput"),
            "default.toml must not hardcode known_tool_registry (#93) — leave it empty to \
             inherit the built-in list"
        );
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
    fn registry_covers_current_cli_tools() {
        // Regression for #85: every tool the issue named as falsely flagged
        // must now be in the default registry.
        let cfg = ClassifierConfig::default();
        for t in [
            "Glob", "TodoWrite", "MultiEdit", "NotebookEdit", "BashOutput",
            "KillShell", "ExitPlanMode", "SlashCommand", "TaskOutput",
            "TaskCreate", "TaskGet", "TaskList", "TaskStop", "TaskUpdate",
        ] {
            assert!(
                cfg.known_tool_registry.contains(t),
                "{t} should be a known tool, not flagged UnknownToolName (#85)"
            );
        }
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
        // MCP system-prompt marker + an mcp__-prefixed tool → pure Mcp surface.
        let p = AgentPrimitives {
            tool_call_count: 1,
            tool_names: vec!["mcp__svc__do".to_string()],
            has_system_prompt: true,
            system_prompt_markers: SystemPromptMarkers::MCP_SERVER,
            ..Default::default()
        };
        let c = classify(&p, &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::Mcp));
    }

    #[test]
    fn surface_mixed_for_mcp_marker_plus_function_tool() {
        // An MCP marker AND a (non-mcp) function-call tool = both surfaces.
        let p = AgentPrimitives {
            tool_call_count: 1,
            tool_names: vec!["custom_tool".to_string()],
            has_system_prompt: true,
            system_prompt_markers: SystemPromptMarkers::MCP_SERVER,
            ..Default::default()
        };
        let c = classify(&p, &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::Mixed));
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
    fn surface_function_call_for_unregistered_tool() {
        // An unrecognized tool name is a function-call tool, NOT Unknown/suspicious
        // — we don't allow-list every agent's vocabulary (#85 follow-up).
        let c = classify(&prims_with_tools(&["mystery_tool", "web_search", "read"]), &cfg());
        assert_eq!(c.tool_surface, Some(ToolSurface::FunctionCall));
        assert!(c.suspicious_signals.is_empty());
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
    fn unrecognized_tools_are_not_flagged_suspicious() {
        // Other agents' tools (openclaw etc.: snake_case, different vocab) must
        // NOT be flagged suspicious — they are ordinary function calls (#85).
        let c = classify(
            &prims_with_tools(&["web_search", "read_file", "memory_get", "sessions_spawn"]),
            &cfg(),
        );
        assert!(c.suspicious_signals.is_empty(), "{:?}", c.suspicious_signals);
        assert_eq!(c.tool_surface, Some(ToolSurface::FunctionCall));
    }

    #[test]
    fn suspicious_empty_for_known_tools() {
        let c = classify(&prims_with_tools(&["Read", "Edit"]), &cfg());
        assert!(c.suspicious_signals.is_empty());
    }

    #[test]
    fn modern_claude_code_toolset_is_clean() {
        // #85: a realistic modern Claude Code tool array must classify as a
        // clean FunctionCall surface — no suspicious signals, not Unknown.
        let c = classify(
            &prims_with_tools(&["Read", "Glob", "TodoWrite", "TaskOutput", "ExitPlanMode"]),
            &cfg(),
        );
        assert!(
            c.suspicious_signals.is_empty(),
            "unexpected suspicious signals: {:?}",
            c.suspicious_signals
        );
        assert_eq!(c.tool_surface, Some(ToolSurface::FunctionCall));
    }
}
