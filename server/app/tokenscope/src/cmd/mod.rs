//! Subcommand implementations. The default (no-subcommand) path runs the
//! capture pipeline and lives in `main.rs`; everything here is for
//! diagnostic / pre-flight subcommands that exit before any pipeline spawn.

pub mod doctor;
pub mod validate;
