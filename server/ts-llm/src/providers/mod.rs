//! Default `Provider` registration.
//!
//! Registration order matters in two ways under two-pass detection:
//!   - Route pass (`classify_route`): if more than one provider would
//!     `Accept` the same request, the earlier one wins. In practice only
//!     the OpenAI pair overlap — `/v1/responses` must precede
//!     `/v1/chat/completions` so the more specific endpoint wins.
//!   - Shape pass (`matches_shape`): same first-match semantics applied to
//!     the `Unknown` candidates. Anthropic goes first because its shape
//!     predicate is more restrictive (requires top-level `system` or
//!     `stop_sequences`); OpenAI Chat is the broadest fallback and must
//!     come last.
//!
//! To add a new provider (Azure / Gemini / vLLM-OpenAI-compat / …):
//!   1. Add a `pub struct ...Provider` implementing `Provider` (with its
//!      `classify_route` + `matches_shape` predicates) in a new module
//!      under `ts_llm::`.
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
