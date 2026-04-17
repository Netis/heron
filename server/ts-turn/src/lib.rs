//! Turn grouping: aggregates `LlmCall` into `LlmTurn` per client session.
//!
//! Header-explicit only — calls without a matching `ClientProfile` do not
//! participate in turn grouping.

pub mod model;
pub mod stage;
pub mod tracker;

pub use model::{LlmTurn, TurnKey, TurnStatus};
pub use stage::spawn_turn_stage;
pub use tracker::{TurnEvent, TurnTracker};
