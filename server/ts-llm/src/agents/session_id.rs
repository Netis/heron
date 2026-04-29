//! Session-id synthesis strategies shared by profiles that derive
//! `session_id` from request/response payloads (currently `generic` and
//! `openclaw`). Not part of any `AgentProfile` trait — callers parse the
//! body's wire-api shape via `crate::wire_apis::*` first and pass the
//! resulting `AssistantSig` here for canonicalization / hashing.
//!
//! Two strategies live here:
//!   1. Tool-id canonicalization: restore the LLM-side `prefix_<rest>`
//!      form when a client strips the underscore between prefix and body
//!      (observed with OpenClaw's GLM-via-OpenAI/JS SDK quirk).
//!   2. FNV-1a text hash: synthesize `gen-<16hex>` from first-user-text +
//!      first-assistant-text when no tool id is available.

use crate::wire_apis::AssistantSig;

/// Tool-id canonicalization. Restores the LLM-side `prefix_<rest>` form
/// when a client has stripped the underscore between the prefix and the
/// id body.
///
/// Observed in the wild: OpenClaw (OpenAI/JS SDK + GLM model) emits
/// `call_d9c1...` over the wire but echoes `calld9c1...` (no underscore)
/// when reflecting `assistant.tool_calls[]` into subsequent
/// `messages` history. Without canonicalization, the same tool id appears
/// as two distinct strings, splitting every session at its first call.
///
/// Returns the input unchanged when no rule applies. Future client quirks
/// (lowercase, prefix swap, truncation) are not handled here — each new
/// normalization should be added as a small targeted patch.
pub fn canonicalize_tool_id(id: &str) -> String {
    const PREFIXES: &[&str] = &["call", "toolu", "fc", "chatcmpl"];
    for p in PREFIXES {
        let Some(after) = id.strip_prefix(p) else { continue };
        if !after.is_empty() && !after.starts_with('_') {
            return format!("{p}_{after}");
        }
    }
    id.to_string()
}

/// Stable 64-bit FNV-1a hash, hex-formatted to 16 chars. Used as the
/// fallback when no tool id is available — combines first user text with
/// first assistant text. Non-crypto by design; we only need stability and
/// speed.
pub fn synth_text_hash(user_text: &str, assistant_text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in user_text.bytes().chain(b"\n".iter().copied()).chain(assistant_text.bytes()) {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// Shared session_id composition: prefer canonicalized tool id (raw form,
/// debuggable against capture data); fall back to `gen-<16hex>` text hash.
///
/// Use `compose_session_id_tracked` when you need to know whether the
/// canonicalization actually modified the tool id (e.g., for metrics).
pub fn compose_session_id(user_text: &str, sig: AssistantSig) -> String {
    compose_session_id_tracked(user_text, sig).0
}

/// Like canonicalize_tool_id but reports whether the input was modified.
/// The caller increments `LlmGenericToolIdCanonicalized` when changed.
pub fn canonicalize_tool_id_tracked(id: &str) -> (String, bool) {
    let canon = canonicalize_tool_id(id);
    let changed = canon != id;
    (canon, changed)
}

/// Returns `(session_id, tool_id_was_canonicalized)`. The boolean lets the
/// caller bump a counter without re-running the canonicalization rule.
pub fn compose_session_id_tracked(user_text: &str, sig: AssistantSig) -> (String, bool) {
    match sig {
        AssistantSig::ToolId(id) => {
            let (canon, changed) = canonicalize_tool_id_tracked(&id);
            (canon, changed)
        }
        AssistantSig::Text(text) => (format!("gen-{}", synth_text_hash(user_text, &text)), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_underscore_present() {
        assert_eq!(canonicalize_tool_id("call_abc"), "call_abc");
    }

    #[test]
    fn inserts_for_call_prefix() {
        assert_eq!(canonicalize_tool_id("calld9c1e9e6617a41ca860562a1"), "call_d9c1e9e6617a41ca860562a1");
    }

    #[test]
    fn inserts_for_toolu_prefix() {
        assert_eq!(canonicalize_tool_id("tooluxyz"), "toolu_xyz");
    }

    #[test]
    fn inserts_for_fc_prefix() {
        assert_eq!(canonicalize_tool_id("fcabc"), "fc_abc");
    }

    #[test]
    fn inserts_for_chatcmpl_prefix() {
        assert_eq!(canonicalize_tool_id("chatcmplabc"), "chatcmpl_abc");
    }

    #[test]
    fn passthrough_unknown_prefix() {
        assert_eq!(canonicalize_tool_id("abc_xyz"), "abc_xyz");
    }

    #[test]
    fn passthrough_empty_after_prefix() {
        assert_eq!(canonicalize_tool_id("call"), "call");
    }

    #[test]
    fn synth_hash_is_stable_and_unique() {
        let a = synth_text_hash("hello", "world");
        let b = synth_text_hash("hello", "world");
        let c = synth_text_hash("hello", "WORLD");
        assert_eq!(a, b, "same input → same hash");
        assert_ne!(a, c, "different input → different hash");
        assert_eq!(a.len(), 16, "16-char hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn compose_tool_id_is_canonicalized() {
        let sid = compose_session_id("hello", AssistantSig::ToolId("calldef".to_string()));
        assert_eq!(sid, "call_def");
    }

    #[test]
    fn compose_text_uses_gen_prefix() {
        let sid = compose_session_id("hello", AssistantSig::Text("world".to_string()));
        assert!(sid.starts_with("gen-"));
        assert_eq!(sid.len(), "gen-".len() + 16);
    }

    #[test]
    fn compose_tracks_canonicalization() {
        let (id, changed) = compose_session_id_tracked("u", AssistantSig::ToolId("call_abc".into()));
        assert_eq!(id, "call_abc");
        assert!(!changed);

        let (id, changed) = compose_session_id_tracked("u", AssistantSig::ToolId("callabc".into()));
        assert_eq!(id, "call_abc");
        assert!(changed);

        let (id, changed) = compose_session_id_tracked("u", AssistantSig::Text("t".into()));
        assert!(id.starts_with("gen-"));
        assert!(!changed);
    }
}
