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
pub mod gemini_aistudio;
pub mod openai;

use crate::wire_api_registry::WireApiRegistry;
use anthropic::AnthropicWireApi;
use gemini_aistudio::GeminiAiStudioWireApi;
use openai::{OpenAiChatWireApi, OpenAiResponsesWireApi};

/// Anthropic currently ships one public chat-style API (Messages). If a
/// second Anthropic wire shape appears later (e.g. a Responses-equivalent),
/// split this into `ANTHROPIC_MESSAGES`, `ANTHROPIC_RESPONSES`, etc. and
/// migrate the stored value.
pub const ANTHROPIC: &str = "anthropic";
pub const OPENAI_CHAT: &str = "openai-chat";
pub const OPENAI_RESPONSES: &str = "openai-responses";

/// Gemini ships at least three known wire shapes from day one: the public
/// AI Studio REST surface (here), the Code Assist OAuth wrap (used by
/// Gemini CLI when logged in with a personal Google account; body wraps
/// `contents` under a `request` envelope), and Vertex AI (different host
/// and auth, body identical to AI Studio). We allocate the namespace up
/// front to avoid storage migration when the OAuth and Vertex variants
/// land, analogous to how `openai-chat` and `openai-responses` already
/// split. Future constants land here as `GEMINI_CODEASSIST` and
/// `GEMINI_VERTEX`; never collapse them back to a bare `gemini`.
pub const GEMINI_AISTUDIO: &str = "gemini-aistudio";

/// Resolve a stored wire-API string back to its `&'static str` constant.
/// Returns `None` for unknown values — callers that need a static wire_api
/// (e.g. to rebuild an `LlmCall` from a DB row) should treat this as "drop
/// the record" rather than invent one.
pub fn by_name(name: &str) -> Option<&'static str> {
    match name {
        ANTHROPIC => Some(ANTHROPIC),
        OPENAI_CHAT => Some(OPENAI_CHAT),
        OPENAI_RESPONSES => Some(OPENAI_RESPONSES),
        GEMINI_AISTUDIO => Some(GEMINI_AISTUDIO),
        _ => None,
    }
}

/// Default registry with all built-in wire APIs.
pub fn build_default_wire_api_registry() -> WireApiRegistry {
    WireApiRegistry::new()
        .with(Box::new(AnthropicWireApi))
        .with(Box::new(OpenAiResponsesWireApi))
        .with(Box::new(GeminiAiStudioWireApi))
        .with(Box::new(OpenAiChatWireApi))
}

/// First-assistant signature extracted from a body parser. Returned by the
/// per-wire-api `first_assistant_sig_from_*` helpers; consumed by callers
/// that synthesize a stable session-id (see `agents/session_id.rs`).
/// The variants reflect the only two anchors the wire APIs surface:
///   - `ToolId(id)`: assistant emitted a tool/function call — `id` is the
///     wire-canonical tool/function call id (`toolu_*`, `call_*`, `fc_*`).
///   - `Text(joined)`: no tool, fall back to the assistant's first text
///     response. Caller hashes this against the user prompt to synthesize
///     `gen-<hex>`.
pub enum AssistantSig {
    ToolId(String),
    Text(String),
}
