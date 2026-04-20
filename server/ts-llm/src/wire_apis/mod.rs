//! Wire-API identifiers and default registration.
//!
//! A "wire API" identifies the HTTP API shape seen on the wire — what JSON
//! schema the request/response bodies follow and which endpoint handles them.
//! It is **not** the vendor: Azure OpenAI, vLLM, Ollama and api.openai.com all
//! speak the same `openai-chat` wire API despite being different vendors.
//! (When TokenScope later needs to distinguish vendors, that will be a
//! separate field sourced from hostname / key prefix / route prefix.)
//!
//! Values use the compound `<vendor>-<api>` form so operators and storage
//! filters stay self-descriptive without needing a separate column. These
//! strings are persisted verbatim to storage via `LlmCall.wire_api` and
//! surface through the API filter UI — changing a value is a **data
//! migration**. Every caller must use these constants rather than raw string
//! literals so a rename touches exactly one place.
//!
//! Registration order matters in two ways under two-pass detection:
//!   - Route pass (`classify_route`): if more than one wire API would
//!     `Accept` the same request, the earlier one wins. In practice only
//!     the OpenAI pair overlap — `/v1/responses` must precede
//!     `/v1/chat/completions` so the more specific endpoint wins.
//!   - Shape pass (`matches_shape`): same first-match semantics applied to
//!     the `Unknown` candidates. Anthropic goes first because its shape
//!     predicate is more restrictive (requires top-level `system` or
//!     `stop_sequences`); OpenAI Chat is the broadest fallback and must
//!     come last.
//!
//! To add a new wire API (Azure / Gemini / vLLM-OpenAI-compat / …):
//!   1. Add a `pub struct ...WireApi` implementing `WireApi` (with its
//!      `classify_route` + `matches_shape` predicates) in a new submodule.
//!   2. Register it here via `.with(Box::new(MyWireApi))`.

pub mod anthropic;
pub mod openai;

use crate::wire_api_registry::WireApiRegistry;
use anthropic::AnthropicMessagesWireApi;
use openai::{OpenAiChatWireApi, OpenAiResponsesWireApi};

pub const ANTHROPIC_MESSAGES: &str = "anthropic-messages";
pub const OPENAI_CHAT: &str = "openai-chat";
pub const OPENAI_RESPONSES: &str = "openai-responses";

/// Default registry with all built-in wire APIs.
pub fn build_default_wire_api_registry() -> WireApiRegistry {
    WireApiRegistry::new()
        .with(Box::new(AnthropicMessagesWireApi))
        .with(Box::new(OpenAiResponsesWireApi))
        .with(Box::new(OpenAiChatWireApi))
}
