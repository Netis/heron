pub mod agents;
pub mod model;
pub mod processor;
pub mod profile;
pub mod stage;
pub mod wire_api_registry;
pub mod wire_apis;

pub use model::WireApi;
pub use processor::build_agent_call_info;
pub use profile::{AgentProfile, AgentProfileRegistry, SessionIdExtraction};
pub use stage::spawn_llm_stage;
pub use wire_api_registry::WireApiRegistry;
