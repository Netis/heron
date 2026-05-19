//! Shared types used across every pipeline crate.
//!
//! Intentionally tiny and dependency-light:
//!
//! * [`config`] — root `AppConfig` loaded from TOML, plus per-stage config
//!   sub-structs (capture / pipeline / turn / metrics / storage / api).
//! * [`error`] — unified `AppError` + `Result` alias used by binaries.
//! * [`internal_metrics`] — lightweight in-process counters for pipeline
//!   self-observability (queue depth, drops, etc.).
//!
//! No stage logic lives here; avoid adding domain types to keep the
//! compile-time coupling surface small.

pub mod config;
pub mod config_edit;
pub mod error;
pub mod internal_metrics;
pub mod path;
pub mod throttle;
pub mod version;
