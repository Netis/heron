//! Stable provider identifiers.
//!
//! These strings are persisted verbatim to storage via `LlmCall.provider` and
//! surface through the API filter UI. Changing a value is a **data migration**
//! — every caller must use these constants rather than raw string literals so
//! a rename touches exactly one place.

pub const ANTHROPIC: &str = "anthropic";
pub const OPENAI: &str = "openai";
pub const OPENAI_RESPONSES: &str = "openai-responses";
