//! Default `Provider` registration.
//!
//! Registration order is **significant**: `ProviderRegistry::detect` returns
//! the first `matches()` hit. Put the most specific path first (Anthropic
//! `/v1/messages` is distinct; OpenAI `/v1/responses` must precede
//! `/v1/chat/completions` since they share POST+Bearer).
//!
//! To add a new provider (Azure / Gemini / vLLM-OpenAI-compat / …):
//!   1. Add a `pub struct ...Provider` implementing `Provider` (with its own
//!      `matches()` detection) in a new module under `ts_llm::`.
//!   2. Register it here via `.with(Box::new(MyProvider))`.

use crate::anthropic::AnthropicProvider;
use crate::openai::{OpenAiChatProvider, OpenAiResponsesProvider};
use crate::provider_registry::ProviderRegistry;

/// Default registry with all built-in providers.
pub fn build_default_provider_registry() -> ProviderRegistry {
    ProviderRegistry::new()
        .with(Box::new(AnthropicProvider))
        .with(Box::new(OpenAiResponsesProvider))
        .with(Box::new(OpenAiChatProvider))
}
