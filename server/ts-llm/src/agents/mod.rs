//! Concrete AgentProfile implementations — one module per supported agent
//! client. To add a new agent: write a new module here, impl `AgentProfile`,
//! and register it in `build_default_registry()` below.

use crate::profile::AgentProfileRegistry;

pub mod claude_cli;
pub mod codex_cli;

/// Default registry with all built-in agent profiles.
pub fn build_default_registry() -> AgentProfileRegistry {
    AgentProfileRegistry::new()
        .with(Box::new(claude_cli::ClaudeCliProfile))
        .with(Box::new(codex_cli::CodexCliProfile))
}
