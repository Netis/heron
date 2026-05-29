//! OpenAI-family wire APIs — Chat Completions (`/v1/chat/completions`) and
//! Responses (`/v1/responses`). Each submodule owns its full request /
//! response / SSE parser; only thin header-classifier helpers are shared
//! (see `shared.rs`). Parse-time field mapping is deliberately split so a
//! change in one API cannot silently affect the other.

pub mod chat;
pub mod responses;
mod shared;

pub use chat::OpenAiChatWireApi;
pub use responses::body_has_terminal_message_only;
pub use responses::body_has_terminal_message_only_value;
pub use responses::OpenAiResponsesWireApi;
