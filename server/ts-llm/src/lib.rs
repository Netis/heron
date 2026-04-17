pub mod model;
pub mod processor;
pub mod profile;
pub mod profiles;
pub mod stage;

// Internal modules — not part of the public API.
pub(crate) mod anthropic;
pub(crate) mod detector;
pub(crate) mod openai;

pub use profile::{ClientProfile, ExtractedIds, ProfileRegistry};
pub use stage::spawn_llm_stage;
