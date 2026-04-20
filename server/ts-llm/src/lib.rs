pub mod model;
pub mod processor;
pub mod profile;
pub mod profiles;
pub mod provider_names;
pub mod provider_registry;
pub mod providers;
pub mod stage;

// Internal modules — not part of the public API.
pub(crate) mod anthropic;
pub(crate) mod openai;

pub use model::Provider;
pub use profile::{ClientProfile, ExtractedIds, ProfileRegistry};
pub use provider_registry::ProviderRegistry;
pub use stage::spawn_llm_stage;
