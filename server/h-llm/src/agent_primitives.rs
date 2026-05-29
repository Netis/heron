//! Profile-extracted facts about a single LLM call. Lives in-memory only
//! (never persisted); the classifier consumes these to derive the fields
//! that are persisted on `LlmCall`.

use bitflags::bitflags;

bitflags! {
    /// Bitflags set by extractors after scanning the system prompt of the
    /// request. Order is stable; add new flags at the end.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct SystemPromptMarkers: u32 {
        const AGENT_LOOP          = 0b0000_0001;
        const REACT_SKELETON      = 0b0000_0010;
        const TOOL_USE_INSTRUCTION = 0b0000_0100;
        const MCP_SERVER          = 0b0000_1000;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentPrimitives {
    /// Count of tool calls visible in this call's request/response. Includes
    /// both function-call envelopes and inline tool_use blocks.
    pub tool_call_count: u32,
    /// Distinct tool names referenced in this call. Deduplicated by extractor;
    /// preserved in order of first appearance.
    pub tool_names: Vec<String>,
    /// True when this request carries a non-empty system prompt.
    pub has_system_prompt: bool,
    /// Markers detected inside the system prompt.
    pub system_prompt_markers: SystemPromptMarkers,
    /// Profile-derived sub-agent marker (e.g. "Task" for Claude Code), if any.
    /// Reuses the existing concept already computed by profiles.
    pub subagent_marker: Option<String>,
    /// True when this call dispatches to a sub-agent (e.g. invokes the `Task` tool).
    /// Distinct from `subagent_marker`, which signals "this call IS a sub-agent".
    pub dispatches_to_subagent: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_combine() {
        let m = SystemPromptMarkers::AGENT_LOOP | SystemPromptMarkers::MCP_SERVER;
        assert!(m.contains(SystemPromptMarkers::AGENT_LOOP));
        assert!(!m.contains(SystemPromptMarkers::REACT_SKELETON));
        assert!(m.contains(SystemPromptMarkers::MCP_SERVER));
    }

    #[test]
    fn primitives_defaults_are_inert() {
        let p = AgentPrimitives::default();
        assert_eq!(p.tool_call_count, 0);
        assert!(p.tool_names.is_empty());
        assert!(!p.has_system_prompt);
        assert!(p.system_prompt_markers.is_empty());
        assert!(p.subagent_marker.is_none());
        assert!(!p.dispatches_to_subagent);
    }
}
