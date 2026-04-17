//! Library surface of the `tokenscope` crate.
//!
//! Only the composition root is re-exported: everything else lives in the
//! per-stage `ts-*` crates. Exposing `Pipeline` lets integration tests under
//! `tests/` drive the full pipeline end-to-end without duplicating wiring.

pub mod pipeline;

pub use pipeline::{Pipeline, StageTask};
