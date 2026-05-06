//! Per-event JSON parse cache, bound to its source at construction.
//!
//! Two construction paths, mutually exclusive:
//!
//!   1. `ParsedJson::from_bytes(body)` — for raw HTTP bodies (request bodies,
//!      non-SSE response bodies). The cache lazy-parses the bytes on first
//!      `get()` and memoizes the result.
//!   2. `ParsedJson::from_value(synthetic)` — for the SSE assembly path,
//!      where the wire impl walks per-event Values and produces a synthetic
//!      response Value directly. No bytes ever exist for that body.
//!
//! `get()` is the only read API. Source binding happens at construction —
//! a cache built from one body's bytes can never be fed bytes from another
//! body, removing the misuse vector that the previous "empty cache + later
//! `from_bytes(&self, &[u8])` / `set(...)`" API allowed.
//!
//! Caches are **per-event**: the processor creates fresh caches inside
//! `on_request` (Start) and inside `on_exchange` (Complete). The two paths
//! do not share state — the same HTTP request body may be parsed once on
//! the Start path and once on the Complete path. Collapsing the two would
//! require a per-call cache map keyed by request id; the savings on the
//! Start path (which only reads `model` and `is_stream`) don't justify it
//! today.

use std::sync::OnceLock;

use bytes::Bytes;
use serde_json::Value;

/// Parse-once cache bound at construction to either raw bytes (lazy) or a
/// pre-built `Value` (eager). See module docs for the contract.
pub enum ParsedJson {
    /// Raw HTTP body, parsed on first `get()`.
    Lazy {
        bytes: Bytes,
        parsed: OnceLock<Option<Value>>,
    },
    /// Pre-built `Value` (e.g. SSE synthetic body). `None` represents
    /// "the wire impl produced no body" and is indistinguishable from a
    /// failed lazy parse for downstream callers.
    Eager(Option<Value>),
}

impl ParsedJson {
    /// Bind the cache to raw HTTP body bytes. Cloning `Bytes` is cheap
    /// (Arc-style refcount on the underlying buffer), so callers usually
    /// pass `request.body.clone()` / `response.body.clone()`.
    pub fn from_bytes(bytes: Bytes) -> Self {
        ParsedJson::Lazy {
            bytes,
            parsed: OnceLock::new(),
        }
    }

    /// Bind the cache to an already-built `Value`. Used by SSE assembly
    /// paths that synthesize the body from per-event Values rather than
    /// parse a contiguous byte buffer.
    pub fn from_value(value: Option<Value>) -> Self {
        ParsedJson::Eager(value)
    }

    /// Read the cached `Value`. For `Lazy`, parses on first call and
    /// memoizes. For `Eager`, returns the bound value directly without
    /// any extra work — no clone, no OnceLock round-trip.
    pub fn get(&self) -> Option<&Value> {
        match self {
            ParsedJson::Lazy { bytes, parsed } => parsed
                .get_or_init(|| serde_json::from_slice(bytes).ok())
                .as_ref(),
            ParsedJson::Eager(v) => v.as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_bytes_parses_on_first_get() {
        let cache = ParsedJson::from_bytes(Bytes::from_static(br#"{"a":1}"#));
        assert_eq!(cache.get(), Some(&json!({"a": 1})));
        // Second call hits the memoized result.
        assert_eq!(cache.get(), Some(&json!({"a": 1})));
    }

    #[test]
    fn from_bytes_memoizes_parse_failure() {
        let cache = ParsedJson::from_bytes(Bytes::from_static(b"not json"));
        assert!(cache.get().is_none());
        assert!(cache.get().is_none());
    }

    #[test]
    fn from_value_returns_prebuilt_value_without_clone() {
        let original = json!({"x": 9, "nested": {"deep": [1, 2, 3]}});
        let cache = ParsedJson::from_value(Some(original.clone()));
        let borrowed = cache.get().expect("eager value");
        assert_eq!(borrowed, &original);
        // Calling again is a free borrow — no parse, no clone.
        assert!(std::ptr::eq(cache.get().unwrap(), borrowed));
    }

    #[test]
    fn from_value_none_is_supported() {
        let cache = ParsedJson::from_value(None);
        assert!(cache.get().is_none());
    }

    #[test]
    fn from_bytes_with_empty_body() {
        let cache = ParsedJson::from_bytes(Bytes::new());
        assert!(cache.get().is_none());
    }
}
