//! Concrete ClientProfile implementations.
//!
//! To add a new client: write a new module here, impl `ClientProfile`, and
//! register it in `build_default_registry()` below.

use crate::profile::ProfileRegistry;

pub mod claude_cli;
pub mod codex_cli;

/// Default registry with all built-in client profiles.
pub fn build_default_registry() -> ProfileRegistry {
    ProfileRegistry::new()
        .with(Box::new(claude_cli::ClaudeCliProfile))
        .with(Box::new(codex_cli::CodexCliProfile))
}
