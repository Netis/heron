# Generic Agent Profile Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add three profile-agnostic `AgentProfile` impls (`generic-anthropic`, `generic-openai-chat`, `generic-openai-responses`) that synthesize a `session_id` from the request/response payload alone, so TokenScope produces `AgentTurn`s for header-less LLM traffic (Python SDKs, OpenClaw, generic curl harnesses).

**Architecture:** Each generic profile is a self-contained file in `server/ts-llm/src/agents/`. Tool ids are canonicalized (LLM-side `call_<hex>` form) before use as `session_id` so client-side normalization (e.g., underscore stripping seen in OpenClaw) does not split sessions. Codex's `body_has_terminal_message_only` migrates from `agents/codex_cli.rs` to `wire_apis/openai/responses.rs` so both `CodexCliProfile` and `GenericOpenAiResponsesProfile` share it. `ts-turn` is unchanged.

**Tech Stack:** Rust 2021, `serde_json` for body parsing, `std::collections::hash_map::DefaultHasher` is *not* used (its random seed makes ids non-deterministic across processes); use a tiny inline FNV-1a 64-bit hasher for stable hashing without adding dependencies.

**Spec:** `docs/superpowers/specs/2026-04-28-generic-agent-profile-design.md`

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `server/ts-common/src/internal_metrics.rs` | modify (add 2 enum variants) | Add `LlmGenericToolIdCanonicalized`, `LlmGenericSessionIdUnsynth` metric variants |
| `server/ts-llm/src/wire_apis/openai/responses.rs` | modify | Add `pub fn body_has_terminal_message_only(response_body: Option<&str>) -> bool` (migrated from codex_cli.rs) |
| `server/ts-llm/src/agents/codex_cli.rs` | modify | `is_turn_terminal` becomes a one-line call into the wire-api helper; affected tests follow the helper |
| `server/ts-llm/src/agents/generic_common.rs` | create | `canonicalize_tool_id` helper + `synth_text_hash` (FNV-1a 64-bit) + `AssistantSig` enum |
| `server/ts-llm/src/agents/generic_anthropic.rs` | create | `GenericAnthropicProfile` |
| `server/ts-llm/src/agents/generic_openai_chat.rs` | create | `GenericOpenAiChatProfile` |
| `server/ts-llm/src/agents/generic_openai_responses.rs` | create | `GenericOpenAiResponsesProfile` |
| `server/ts-llm/src/agents/mod.rs` | modify | `pub mod` declarations + 3 new `.with(...)` lines in `build_default_registry` |
| `server/ts-llm/src/processor.rs` | modify | Counter increments for `tool_id_canonicalized` (read from a thread-local set during canonicalization) and `session_id_unsynth` (when `extract_ids` returns `None` for a generic-* match candidate) |
| `server/ts-llm/src/stage.rs` | modify | Register the two new metrics on the worker so `processor` can increment them |
| `server/ts-turn/tests/integration.rs` | modify | New `generic_anthropic_two_call_session` test |

---

## Task 1: Add Internal Metrics

**Files:**
- Modify: `server/ts-common/src/internal_metrics.rs:164-168` (extend the LLM section)
- Modify: `server/ts-common/src/internal_metrics.rs:687-700` (the existing `register_worker` test patterns — no change needed; just confirms style)

- [ ] **Step 1: Read the current LLM metric section** (lines 164–168).

- [ ] **Step 2: Add two variants** — insert after `LlmCallsWithoutAgent`:

```rust
    LlmGenericToolIdCanonicalized => { kind: Counter, group: Llm, short: "generic_tool_id_canon"   },
    LlmGenericSessionIdUnsynth    => { kind: Counter, group: Llm, short: "generic_session_unsynth" },
```

- [ ] **Step 3: Run `cargo test -p ts-common --lib`** — expect: PASS (the macro just adds enum variants; existing tests unaffected).

- [ ] **Step 4: Commit**

```bash
git add server/ts-common/src/internal_metrics.rs
git commit -m "feat(internal-metrics): add generic-profile counters

Adds LlmGenericToolIdCanonicalized and LlmGenericSessionIdUnsynth so the
upcoming generic-* agent profiles can report tool-id normalization rate
and session synthesis failures."
```

---

## Task 2: Migrate `body_has_terminal_message_only` to Wire-API Layer

**Files:**
- Modify: `server/ts-llm/src/wire_apis/openai/responses.rs` (add pub helper at end of file)
- Modify: `server/ts-llm/src/agents/codex_cli.rs:124-156` (replace inline impl with helper call)

- [ ] **Step 1: Write a failing test in `wire_apis/openai/responses.rs`** at the end of the file (add `#[cfg(test)] mod terminal_helper_tests` if no test mod yet, else extend it):

```rust
#[cfg(test)]
mod terminal_helper_tests {
    use super::*;

    #[test]
    fn terminal_when_output_only_has_message() {
        let body = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}]}"#;
        assert!(body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_function_call_present() {
        let body = r#"{"output":[{"type":"message"},{"type":"function_call","call_id":"fc_1"}]}"#;
        assert!(!body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_no_message() {
        let body = r#"{"output":[{"type":"reasoning"}]}"#;
        assert!(!body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_no_body() {
        assert!(!body_has_terminal_message_only(None));
    }

    #[test]
    fn not_terminal_when_malformed_json() {
        assert!(!body_has_terminal_message_only(Some("garbage")));
    }
}
```

- [ ] **Step 2: Run** `cargo test -p ts-llm --lib wire_apis::openai::responses::terminal_helper_tests` — expect: FAIL with "function `body_has_terminal_message_only` not found".

- [ ] **Step 3: Add the helper at the bottom of `wire_apis/openai/responses.rs`** (just above the test module):

```rust
/// Decide whether an OpenAI Responses body represents a terminal turn
/// (agent done, no more tool roundtrips pending).
///
/// Logic: scan `response.output[]`. Any item whose `type` ends with `_call`
/// (e.g., `function_call`, `custom_tool_call`, `local_shell_call`, MCP
/// variants) means the agent will execute a tool and re-POST → not terminal.
/// `message` items count as the final answer; `reasoning` is ignored.
/// Return true iff at least one `message` is present and no `*_call` is.
///
/// Used by both `CodexCliProfile` and `GenericOpenAiResponsesProfile` —
/// the OpenAI Responses protocol always sets `status: "completed"` on
/// successful API calls regardless of whether the agent continues, so the
/// wire-api `finish_reason` is unreliable for turn-boundary purposes. This
/// helper is the authoritative override.
pub fn body_has_terminal_message_only(response_body: Option<&str>) -> bool {
    let Some(body) = response_body else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let Some(output) = v.get("output").and_then(|o| o.as_array()) else {
        return false;
    };
    let mut has_message = false;
    for item in output {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("message") => has_message = true,
            Some("reasoning") => {}
            Some(t) if t.ends_with("_call") => return false,
            _ => {}
        }
    }
    has_message
}
```

- [ ] **Step 4: Run** the same test command — expect: PASS (5/5).

- [ ] **Step 5: Update `codex_cli.rs::is_turn_terminal`** — replace the body of `fn is_turn_terminal` (lines 124–156) with:

```rust
    fn is_turn_terminal(&self, call: &LlmCall, _wire_apis: &WireApiRegistry) -> bool {
        // OpenAI Responses' `status: "completed"` cannot distinguish "agent
        // done" from "tool roundtrip pending" — delegate to the wire-api
        // helper that inspects `response.output[]` directly. Override does
        // NOT fall back to the trait default; the wire-api signal is unusable.
        crate::wire_apis::openai::responses::body_has_terminal_message_only(
            call.response_body.as_deref(),
        )
    }
```

- [ ] **Step 6: Run** `cargo test -p ts-llm --lib agents::codex_cli` — expect: PASS (existing codex tests still cover the integration; the inner-logic tests now live in the helper test module).

- [ ] **Step 7: Run** `cargo build -p ts-llm` — expect: clean compile (no unused-import warnings; if `serde_json::Value` becomes unused in `codex_cli.rs`, remove its `use` line).

- [ ] **Step 8: Commit**

```bash
git add server/ts-llm/src/wire_apis/openai/responses.rs server/ts-llm/src/agents/codex_cli.rs
git commit -m "refactor(wire-apis): hoist Responses terminal-output helper

Moves body_has_terminal_message_only from agents/codex_cli.rs to
wire_apis/openai/responses.rs and makes it pub. Both CodexCliProfile and
the upcoming GenericOpenAiResponsesProfile rely on this same logic — the
helper belongs on the wire-api layer, not in one specific agent profile."
```

---

## Task 3: Create `generic_common.rs` (Helpers)

**Files:**
- Create: `server/ts-llm/src/agents/generic_common.rs`

- [ ] **Step 1: Write failing tests** for `canonicalize_tool_id`. Create the file with the test module first:

```rust
//! Shared helpers for `generic-*` profiles. Not exposed as part of any
//! `AgentProfile` trait — each generic profile parses its own JSON shape
//! and only reaches in here for cross-profile canonicalization / hashing.

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

/// Internal classification of the first assistant message's signature.
/// Generic profiles produce one of these from request body (call #2+) or
/// response body (call #1) and feed it to `compose_session_id`.
pub enum AssistantSig {
    ToolId(String),
    Text(String),
}

/// Shared session_id composition: prefer canonicalized tool id (raw form,
/// debuggable against capture data); fall back to `gen-<16hex>` text hash.
pub fn compose_session_id(user_text: &str, sig: AssistantSig) -> String {
    match sig {
        AssistantSig::ToolId(id) => canonicalize_tool_id(&id),
        AssistantSig::Text(text) => format!("gen-{}", synth_text_hash(user_text, &text)),
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
}
```

- [ ] **Step 2: Add `pub mod generic_common;` to `server/ts-llm/src/agents/mod.rs`** (at the top with the other `pub mod` lines).

- [ ] **Step 3: Run** `cargo test -p ts-llm --lib agents::generic_common` — expect: PASS (10/10).

- [ ] **Step 4: Commit**

```bash
git add server/ts-llm/src/agents/generic_common.rs server/ts-llm/src/agents/mod.rs
git commit -m "feat(agents): add generic_common helpers

canonicalize_tool_id restores the LLM-side prefix_<rest> form when the
client has stripped the underscore (OpenClaw observed). synth_text_hash
is a stable 64-bit FNV-1a used as the no-tool-id fallback. compose_session_id
ties them together; AssistantSig enum tags the source.

Used by the upcoming three generic-* profiles."
```

---

## Task 4: Create `generic_anthropic.rs`

**Files:**
- Create: `server/ts-llm/src/agents/generic_anthropic.rs`

- [ ] **Step 1: Write failing tests** for the profile. Start with the file shell + tests:

```rust
//! Generic Anthropic Messages profile — matches /v1/messages traffic that
//! does NOT carry claude-cli's user-agent. Synthesizes session_id from
//! the messages history (call #2+) or response body (call #1). No
//! sub-agent or auxiliary classification — those signals are
//! claude-cli-specific.

use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_apis as wa;
use serde_json::Value;

use super::generic_common::{compose_session_id, AssistantSig};

pub struct GenericAnthropicProfile;

fn first_user_text(msgs: &[Value]) -> Option<String> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        match m.get("content")? {
            Value::String(s) if !s.trim().is_empty() => return Some(s.clone()),
            Value::Array(blocks) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
                    .collect();
                if !parts.is_empty() {
                    return Some(parts.join("\n"));
                }
            }
            _ => {}
        }
    }
    None
}

fn first_assistant_sig_from_request(msgs: &[Value]) -> Option<AssistantSig> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let blocks = m.get("content")?.as_array()?;
        // Prefer first tool_use id.
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                    return Some(AssistantSig::ToolId(id.to_string()));
                }
            }
        }
        // Fall back to joined text.
        let parts: Vec<String> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect();
        if !parts.is_empty() {
            return Some(AssistantSig::Text(parts.join("\n")));
        }
    }
    None
}

fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let v: Value = serde_json::from_str(body).ok()?;
    let blocks = v.get("content")?.as_array()?;
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                return Some(AssistantSig::ToolId(id.to_string()));
            }
        }
    }
    let parts: Vec<String> = blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(AssistantSig::Text(parts.join("\n")))
    }
}

impl AgentProfile for GenericAnthropicProfile {
    fn name(&self) -> &'static str {
        "generic-anthropic"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        call.wire_api == wa::ANTHROPIC
    }

    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let msgs = v.get("messages")?.as_array()?;
        let user_text = first_user_text(msgs)?;
        let sig = first_assistant_sig_from_request(msgs)
            .or_else(|| call.response_body.as_deref().and_then(first_assistant_sig_from_response))?;
        Some(ExtractedIds {
            session_id: compose_session_id(&user_text, sig),
        })
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        // Last user message contains at least one non-tool_result block?
        // No <system-reminder> stripping (that's claude-cli-specific).
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        match last.get("content")? {
            Value::String(s) => Some(!s.trim().is_empty()),
            Value::Array(blocks) => Some(blocks.iter().any(|b| {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => false,
                    Some("text") => b.get("text").and_then(|t| t.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false),
                    Some(_) => true, // image, future block types — count as user-visible
                    None => false,
                }
            })),
            _ => None,
        }
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        // Last user message text (no system-reminder stripping).
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return None;
        }
        let raw = match last.get("content")? {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
                    .collect();
                parts.join("\n")
            }
            _ => return None,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let blocks = v.get("content")?.as_array()?;
        let parts: Vec<String> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect();
        let joined = parts.join("\n");
        if joined.trim().is_empty() {
            None
        } else {
            Some(joined)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(headers: Vec<(&str, &str)>, req: Option<&str>, resp: Option<&str>) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api: wa::ANTHROPIC,
            model: "claude".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/messages".into(),
            is_stream: true,
            request_body: req.map(str::to_string),
            status_code: None,
            finish_reason: None,
            response_body: resp.map(str::to_string),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            response_headers: vec![],
        }
    }

    #[test]
    fn matches_anthropic_no_ua_required() {
        let c = call_with(vec![], None, None);
        assert!(GenericAnthropicProfile.matches(&c));
    }

    #[test]
    fn does_not_match_other_wire_api() {
        let mut c = call_with(vec![], None, None);
        c.wire_api = wa::OPENAI_CHAT;
        assert!(!GenericAnthropicProfile.matches(&c));
    }

    #[test]
    fn extract_ids_call_n_with_tool_history() {
        let req = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"ok"}]}
        ]}"#;
        let c = call_with(vec![], Some(req), None);
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "toolu_abc");
    }

    #[test]
    fn extract_ids_call_n_with_text_only_history() {
        let req = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"text","text":"hello there"}]},
            {"role":"user","content":[{"type":"text","text":"more"}]}
        ]}"#;
        let c = call_with(vec![], Some(req), None);
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
        assert_eq!(ids.session_id.len(), "gen-".len() + 16);
    }

    #[test]
    fn extract_ids_call_1_tool_in_response() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let resp = r#"{"content":[{"type":"tool_use","id":"toolu_xyz","name":"Read","input":{}}]}"#;
        let c = call_with(vec![], Some(req), Some(resp));
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "toolu_xyz");
    }

    #[test]
    fn extract_ids_call_1_text_in_response() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let resp = r#"{"content":[{"type":"text","text":"hello there"}]}"#;
        let c = call_with(vec![], Some(req), Some(resp));
        let ids = GenericAnthropicProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_call_1_and_n_match() {
        // Call #1 sees response_body; call #2 sees the same content echoed in messages[1].
        let resp = r#"{"content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]}"#;
        let req1 = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"prompt"}]}]}"#;
        let req2 = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"prompt"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"toolu_same","name":"R","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_same","content":"ok"}]}
        ]}"#;
        let c1 = call_with(vec![], Some(req1), Some(resp));
        let c2 = call_with(vec![], Some(req2), None);
        let id1 = GenericAnthropicProfile.extract_ids(&c1).unwrap().session_id;
        let id2 = GenericAnthropicProfile.extract_ids(&c2).unwrap().session_id;
        assert_eq!(id1, id2, "call #1 and call #2 must synthesize same session_id");
    }

    #[test]
    fn extract_ids_call_1_with_normalized_tool_id() {
        // Response stream emits canonical form; some hypothetical client strips the underscore in echo.
        let resp = r#"{"content":[{"type":"tool_use","id":"toolu_abc","name":"R","input":{}}]}"#;
        let req2 = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"x"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"tooluabc","name":"R","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"tooluabc","content":"ok"}]}
        ]}"#;
        let c1 = call_with(vec![], Some(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#), Some(resp));
        let c2 = call_with(vec![], Some(req2), None);
        assert_eq!(
            GenericAnthropicProfile.extract_ids(&c1).unwrap().session_id,
            GenericAnthropicProfile.extract_ids(&c2).unwrap().session_id,
        );
    }

    #[test]
    fn extract_ids_none_when_first_call_no_response() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let c = call_with(vec![], Some(req), None);
        assert!(GenericAnthropicProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn extract_ids_none_when_malformed_json() {
        let c = call_with(vec![], Some("garbage"), None);
        assert!(GenericAnthropicProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn is_user_turn_start_text() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
        let c = call_with(vec![], Some(req), None);
        assert_eq!(GenericAnthropicProfile.is_user_turn_start(&c), Some(true));
    }

    #[test]
    fn is_user_turn_start_tool_result_only() {
        let req = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
        let c = call_with(vec![], Some(req), None);
        assert_eq!(GenericAnthropicProfile.is_user_turn_start(&c), Some(false));
    }
}
```

- [ ] **Step 2: Add `pub mod generic_anthropic;` to `server/ts-llm/src/agents/mod.rs`** (don't register in builder yet — that's Task 7).

- [ ] **Step 3: Run** `cargo test -p ts-llm --lib agents::generic_anthropic` — expect: PASS (12/12).

- [ ] **Step 4: Commit**

```bash
git add server/ts-llm/src/agents/generic_anthropic.rs server/ts-llm/src/agents/mod.rs
git commit -m "feat(agents): add GenericAnthropicProfile

Synthesizes session_id from /v1/messages traffic without claude-cli UA.
Falls back to response_body for call #1; messages history echo for #2+.
No system-reminder stripping (that's claude-cli scaffolding); no sub-agent
classification (no equivalent generic signal)."
```

---

## Task 5: Create `generic_openai_chat.rs`

**Files:**
- Create: `server/ts-llm/src/agents/generic_openai_chat.rs`

- [ ] **Step 1: Write failing tests + the profile.** Create the file:

```rust
//! Generic OpenAI Chat Completions profile — matches /v1/chat/completions
//! traffic from any client. Synthesizes session_id from messages history
//! (call #2+) or response_body (call #1).

use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_apis as wa;
use serde_json::Value;

use super::generic_common::{compose_session_id, AssistantSig};

pub struct GenericOpenAiChatProfile;

fn user_content_to_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    let t = b.get("type").and_then(|v| v.as_str())?;
                    if t == "text" || t == "input_text" {
                        b.get("text").and_then(|v| v.as_str()).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect();
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        _ => None,
    }
}

fn first_user_text(msgs: &[Value]) -> Option<String> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        if let Some(t) = m.get("content").and_then(user_content_to_text) {
            return Some(t);
        }
    }
    None
}

fn first_assistant_sig_from_request(msgs: &[Value]) -> Option<AssistantSig> {
    for m in msgs {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(arr) = m.get("tool_calls").and_then(|v| v.as_array()) {
            if let Some(id) = arr.first().and_then(|tc| tc.get("id")).and_then(|v| v.as_str()) {
                return Some(AssistantSig::ToolId(id.to_string()));
            }
        }
        if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
            if !c.trim().is_empty() {
                return Some(AssistantSig::Text(c.to_string()));
            }
        }
    }
    None
}

fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let v: Value = serde_json::from_str(body).ok()?;
    let msg = v.get("choices")?.get(0)?.get("message")?;
    if let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        if let Some(id) = arr.first().and_then(|tc| tc.get("id")).and_then(|v| v.as_str()) {
            return Some(AssistantSig::ToolId(id.to_string()));
        }
    }
    if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
        if !c.trim().is_empty() {
            return Some(AssistantSig::Text(c.to_string()));
        }
    }
    None
}

impl AgentProfile for GenericOpenAiChatProfile {
    fn name(&self) -> &'static str {
        "generic-openai-chat"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        call.wire_api == wa::OPENAI_CHAT
    }

    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let msgs = v.get("messages")?.as_array()?;
        let user_text = first_user_text(msgs)?;
        let sig = first_assistant_sig_from_request(msgs)
            .or_else(|| call.response_body.as_deref().and_then(first_assistant_sig_from_response))?;
        Some(ExtractedIds {
            session_id: compose_session_id(&user_text, sig),
        })
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        // Last message is role=user with non-empty content (user content blocks
        // in Chat Completions don't have a "tool_result" type — tool results
        // are role=tool messages, which are not user-turn-start by definition).
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        Some(last.get("content").and_then(user_content_to_text).is_some())
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let last = v.get("messages")?.as_array()?.last()?;
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return None;
        }
        last.get("content").and_then(user_content_to_text)
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let c = v.get("choices")?.get(0)?.get("message")?.get("content")?.as_str()?;
        if c.trim().is_empty() {
            None
        } else {
            Some(c.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(req: Option<&str>, resp: Option<&str>) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/chat/completions".into(),
            is_stream: true,
            request_body: req.map(str::to_string),
            status_code: None,
            finish_reason: None,
            response_body: resp.map(str::to_string),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        }
    }

    #[test]
    fn matches_chat_only() {
        let mut c = call_with(None, None);
        assert!(GenericOpenAiChatProfile.matches(&c));
        c.wire_api = wa::ANTHROPIC;
        assert!(!GenericOpenAiChatProfile.matches(&c));
    }

    #[test]
    fn extract_ids_call_n_with_tool_history() {
        let req = r#"{"messages":[
            {"role":"system","content":"you are helpful"},
            {"role":"user","content":"hi"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_abc","content":"ok"}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiChatProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "call_abc");
    }

    #[test]
    fn extract_ids_call_1_tool_in_response_canonicalized() {
        // Simulate OpenClaw: tool_id without underscore in echo, but response
        // (call #1 fallback path) gives canonical form. Both must produce same id.
        let req1 = r#"{"messages":[{"role":"user","content":"x"}]}"#;
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#;
        let req2 = r#"{"messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"callabc","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"callabc","content":"ok"}
        ]}"#;
        let c1 = call_with(Some(req1), Some(resp));
        let c2 = call_with(Some(req2), None);
        let id1 = GenericOpenAiChatProfile.extract_ids(&c1).unwrap().session_id;
        let id2 = GenericOpenAiChatProfile.extract_ids(&c2).unwrap().session_id;
        assert_eq!(id1, "call_abc");
        assert_eq!(id1, id2, "call #1 (canonical) and call #2 (stripped) must canonicalize to same id");
    }

    #[test]
    fn extract_ids_call_n_with_text_only_history() {
        let req = r#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"hello"},
            {"role":"user","content":"more"}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiChatProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_call_1_text_in_response() {
        let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let c = call_with(Some(req), Some(resp));
        assert!(GenericOpenAiChatProfile.extract_ids(&c).unwrap().session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_none_when_first_call_no_response() {
        let req = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let c = call_with(Some(req), None);
        assert!(GenericOpenAiChatProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn extract_ids_none_when_malformed_json() {
        let c = call_with(Some("garbage"), None);
        assert!(GenericOpenAiChatProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn first_user_skips_system() {
        // OpenAI Chat commonly puts system at index 0 — first user text walker
        // must skip it.
        let req = r#"{"messages":[
            {"role":"system","content":"you are X"},
            {"role":"user","content":"actual prompt"}
        ]}"#;
        let v: Value = serde_json::from_str(req).unwrap();
        let msgs = v.get("messages").unwrap().as_array().unwrap();
        assert_eq!(first_user_text(msgs).as_deref(), Some("actual prompt"));
    }

    #[test]
    fn is_user_turn_start_text() {
        let req = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
        assert_eq!(GenericOpenAiChatProfile.is_user_turn_start(&call_with(Some(req), None)), Some(true));
    }

    #[test]
    fn is_user_turn_start_false_when_last_is_tool() {
        let req = r#"{"messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_a","content":"ok"}
        ]}"#;
        assert_eq!(GenericOpenAiChatProfile.is_user_turn_start(&call_with(Some(req), None)), Some(false));
    }
}
```

- [ ] **Step 2: Add `pub mod generic_openai_chat;`** to `server/ts-llm/src/agents/mod.rs`.

- [ ] **Step 3: Run** `cargo test -p ts-llm --lib agents::generic_openai_chat` — expect: PASS (10/10).

- [ ] **Step 4: Commit**

```bash
git add server/ts-llm/src/agents/generic_openai_chat.rs server/ts-llm/src/agents/mod.rs
git commit -m "feat(agents): add GenericOpenAiChatProfile

Synthesizes session_id from /v1/chat/completions traffic. Tool_call ids
are canonicalized to LLM-side form so client-side normalization (observed
in OpenClaw: underscore stripped between response stream and history echo)
does not split sessions."
```

---

## Task 6: Create `generic_openai_responses.rs`

**Files:**
- Create: `server/ts-llm/src/agents/generic_openai_responses.rs`

- [ ] **Step 1: Write failing tests + the profile:**

```rust
//! Generic OpenAI Responses profile — matches /v1/responses traffic that
//! does NOT carry codex-tui's UA or X-Codex-Turn-Metadata header.
//!
//! Uses the same `body_has_terminal_message_only` helper as CodexCliProfile
//! for `is_turn_terminal`, since the wire-api `status: "completed"` is
//! unreliable for this protocol regardless of which client emitted the call.

use crate::model::LlmCall;
use crate::profile::{AgentProfile, ExtractedIds};
use crate::wire_api_registry::WireApiRegistry;
use crate::wire_apis as wa;
use serde_json::Value;

use super::generic_common::{compose_session_id, AssistantSig};

pub struct GenericOpenAiResponsesProfile;

fn message_text(item: &Value) -> Option<String> {
    let c = item.get("content")?;
    match c {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    let t = b.get("type").and_then(|v| v.as_str())?;
                    if t == "text" || t == "input_text" || t == "output_text" {
                        b.get("text").and_then(|v| v.as_str()).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect();
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        _ => None,
    }
}

fn first_user_text(items: &[Value]) -> Option<String> {
    for it in items {
        if it.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        if it.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        if let Some(t) = message_text(it) {
            return Some(t);
        }
    }
    None
}

fn first_assistant_sig_from_input(items: &[Value]) -> Option<AssistantSig> {
    let mut tool_id: Option<String> = None;
    let mut text: Option<String> = None;
    for it in items {
        let t = it.get("type").and_then(|v| v.as_str());
        if tool_id.is_none() && t == Some("function_call") {
            if let Some(id) = it.get("call_id").and_then(|v| v.as_str()) {
                tool_id = Some(id.to_string());
            }
        }
        if text.is_none()
            && t == Some("message")
            && it.get("role").and_then(|v| v.as_str()) == Some("assistant")
        {
            if let Some(t) = message_text(it) {
                text = Some(t);
            }
        }
        if tool_id.is_some() && text.is_some() {
            break;
        }
    }
    if let Some(id) = tool_id {
        Some(AssistantSig::ToolId(id))
    } else {
        text.map(AssistantSig::Text)
    }
}

fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let v: Value = serde_json::from_str(body).ok()?;
    let output = v.get("output")?.as_array()?;
    first_assistant_sig_from_input(output)
}

impl AgentProfile for GenericOpenAiResponsesProfile {
    fn name(&self) -> &'static str {
        "generic-openai-responses"
    }

    fn matches(&self, call: &LlmCall) -> bool {
        call.wire_api == wa::OPENAI_RESPONSES
    }

    fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        // input may be array (full mode) or string (simplified mode).
        let (user_text, sig_from_input) = match v.get("input")? {
            Value::Array(items) => (first_user_text(items), first_assistant_sig_from_input(items)),
            Value::String(s) if !s.trim().is_empty() => (Some(s.clone()), None),
            _ => (None, None),
        };
        let user_text = user_text?;
        let sig = sig_from_input
            .or_else(|| call.response_body.as_deref().and_then(first_assistant_sig_from_response))?;
        Some(ExtractedIds {
            session_id: compose_session_id(&user_text, sig),
        })
    }

    fn is_user_turn_start(&self, call: &LlmCall) -> Option<bool> {
        // Last item in input is type=message, role=user with non-empty text?
        // function_call_output items break user-turn-start.
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let items = match v.get("input")? {
            Value::Array(items) => items,
            Value::String(s) => return Some(!s.trim().is_empty()),
            _ => return None,
        };
        let last = items.last()?;
        let t = last.get("type").and_then(|v| v.as_str());
        if t != Some("message") || last.get("role").and_then(|r| r.as_str()) != Some("user") {
            return Some(false);
        }
        Some(message_text(last).is_some())
    }

    fn extract_user_input(&self, call: &LlmCall) -> Option<String> {
        let body = call.request_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        match v.get("input")? {
            Value::Array(items) => {
                // Reverse-walk for the latest type=message, role=user.
                for it in items.iter().rev() {
                    if it.get("type").and_then(|v| v.as_str()) != Some("message") {
                        continue;
                    }
                    if it.get("role").and_then(|v| v.as_str()) != Some("user") {
                        continue;
                    }
                    if let Some(t) = message_text(it) {
                        return Some(t);
                    }
                }
                None
            }
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    fn extract_assistant_text(&self, call: &LlmCall) -> Option<String> {
        let body = call.response_body.as_deref()?;
        let v: Value = serde_json::from_str(body).ok()?;
        let output = v.get("output")?.as_array()?;
        for it in output {
            if it.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            if let Some(t) = message_text(it) {
                return Some(t);
            }
        }
        None
    }

    fn is_turn_terminal(&self, call: &LlmCall, _wire_apis: &WireApiRegistry) -> bool {
        // Same protocol-level reasoning as CodexCliProfile.
        crate::wire_apis::openai::responses::body_has_terminal_message_only(
            call.response_body.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use std::net::IpAddr;

    fn call_with(req: Option<&str>, resp: Option<&str>) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api: wa::OPENAI_RESPONSES,
            model: "gpt".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/responses".into(),
            is_stream: true,
            request_body: req.map(str::to_string),
            status_code: None,
            finish_reason: None,
            response_body: resp.map(str::to_string),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        }
    }

    #[test]
    fn matches_responses_only() {
        let mut c = call_with(None, None);
        assert!(GenericOpenAiResponsesProfile.matches(&c));
        c.wire_api = wa::OPENAI_CHAT;
        assert!(!GenericOpenAiResponsesProfile.matches(&c));
    }

    #[test]
    fn extract_ids_call_n_with_function_call() {
        // Codex-shape input: developer + user + reasoning + assistant + function_call.
        let req = r#"{"input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"sys"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"reasoning","summary":[],"content":[]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_xyz"}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiResponsesProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "fc_xyz", "function_call.call_id wins over assistant text");
    }

    #[test]
    fn extract_ids_call_1_function_call_in_response() {
        let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let resp = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_abc"}]}"#;
        let c = call_with(Some(req), Some(resp));
        let ids = GenericOpenAiResponsesProfile.extract_ids(&c).unwrap();
        assert_eq!(ids.session_id, "fc_abc");
    }

    #[test]
    fn extract_ids_call_1_and_n_match() {
        let req1 = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]}]}"#;
        let resp = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"}]}"#;
        let req2 = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_same"},
            {"type":"function_call_output","call_id":"fc_same","output":"ok"}
        ]}"#;
        let c1 = call_with(Some(req1), Some(resp));
        let c2 = call_with(Some(req2), None);
        assert_eq!(
            GenericOpenAiResponsesProfile.extract_ids(&c1).unwrap().session_id,
            GenericOpenAiResponsesProfile.extract_ids(&c2).unwrap().session_id,
        );
    }

    #[test]
    fn extract_ids_call_n_with_text_only_assistant() {
        let req = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}
        ]}"#;
        let c = call_with(Some(req), None);
        let ids = GenericOpenAiResponsesProfile.extract_ids(&c).unwrap();
        assert!(ids.session_id.starts_with("gen-"));
    }

    #[test]
    fn extract_ids_input_string_mode_treats_as_call_1() {
        // Simplified mode: input is a string. No assistant in input → falls through to response.
        let req = r#"{"input":"just a prompt"}"#;
        let resp = r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_simple"}]}"#;
        let c = call_with(Some(req), Some(resp));
        assert_eq!(GenericOpenAiResponsesProfile.extract_ids(&c).unwrap().session_id, "fc_simple");
    }

    #[test]
    fn extract_ids_none_when_first_call_no_response() {
        let req = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let c = call_with(Some(req), None);
        assert!(GenericOpenAiResponsesProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn extract_ids_none_when_malformed_json() {
        let c = call_with(Some("garbage"), None);
        assert!(GenericOpenAiResponsesProfile.extract_ids(&c).is_none());
    }

    #[test]
    fn is_turn_terminal_delegates_to_helper() {
        // message-only output → terminal.
        let resp = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}"#;
        let mut c = call_with(None, Some(resp));
        let wires = crate::wire_apis::build_default_wire_api_registry();
        assert!(GenericOpenAiResponsesProfile.is_turn_terminal(&c, &wires));
        // function_call → not terminal.
        c.response_body = Some(r#"{"output":[{"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"}]}"#.to_string());
        assert!(!GenericOpenAiResponsesProfile.is_turn_terminal(&c, &wires));
    }

    #[test]
    fn is_user_turn_start_last_user_message() {
        let req = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
            {"type":"function_call_output","call_id":"fc_a","output":"ok"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"more"}]}
        ]}"#;
        assert_eq!(GenericOpenAiResponsesProfile.is_user_turn_start(&call_with(Some(req), None)), Some(true));
    }

    #[test]
    fn is_user_turn_start_false_when_last_is_function_call_output() {
        let req = r#"{"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_a"},
            {"type":"function_call_output","call_id":"fc_a","output":"ok"}
        ]}"#;
        assert_eq!(GenericOpenAiResponsesProfile.is_user_turn_start(&call_with(Some(req), None)), Some(false));
    }
}
```

- [ ] **Step 2: Add `pub mod generic_openai_responses;`** to `server/ts-llm/src/agents/mod.rs`.

- [ ] **Step 3: Run** `cargo test -p ts-llm --lib agents::generic_openai_responses` — expect: PASS (11/11).

- [ ] **Step 4: Commit**

```bash
git add server/ts-llm/src/agents/generic_openai_responses.rs server/ts-llm/src/agents/mod.rs
git commit -m "feat(agents): add GenericOpenAiResponsesProfile

Synthesizes session_id from /v1/responses traffic without codex headers.
Reuses body_has_terminal_message_only from wire_apis layer (same protocol
quirk codex-cli works around). Handles both array and simplified-string
input modes."
```

---

## Task 7: Wire Profiles into Registry

**Files:**
- Modify: `server/ts-llm/src/agents/mod.rs:11-15` (the `build_default_registry` function)

- [ ] **Step 1: Read current mod.rs** to confirm shape.

- [ ] **Step 2: Replace the `build_default_registry` body** with:

```rust
/// Default registry with all built-in agent profiles.
///
/// Order is priority — first match wins. Specific profiles (claude-cli,
/// codex-cli) come first; generic profiles catch traffic without
/// distinguishing client headers.
pub fn build_default_registry() -> AgentProfileRegistry {
    AgentProfileRegistry::new()
        .with(Box::new(claude_cli::ClaudeCliProfile))
        .with(Box::new(codex_cli::CodexCliProfile))
        .with(Box::new(generic_anthropic::GenericAnthropicProfile))
        .with(Box::new(generic_openai_chat::GenericOpenAiChatProfile))
        .with(Box::new(generic_openai_responses::GenericOpenAiResponsesProfile))
}
```

- [ ] **Step 3: Add registry-priority tests** at the bottom of `server/ts-llm/src/agents/mod.rs`:

```rust
#[cfg(test)]
mod priority_tests {
    use super::*;
    use crate::model::{ApiType, LlmCall};
    use crate::wire_apis as wa;
    use std::net::IpAddr;

    fn call_with(wire_api: &'static str, headers: Vec<(&str, &str)>) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "c".into(),
            wire_api,
            model: "m".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/".into(),
            is_stream: true,
            request_body: None,
            status_code: None,
            finish_reason: None,
            response_body: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            response_headers: vec![],
        }
    }

    #[test]
    fn claude_cli_wins_over_generic_anthropic() {
        let reg = build_default_registry();
        let c = call_with(wa::ANTHROPIC, vec![("User-Agent", "claude-cli/2.1.98 (cli)")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("claude-cli"));
    }

    #[test]
    fn generic_anthropic_catches_no_ua_anthropic() {
        let reg = build_default_registry();
        let c = call_with(wa::ANTHROPIC, vec![("User-Agent", "python/3.12 anthropic/0.40")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic-anthropic"));
    }

    #[test]
    fn codex_cli_wins_over_generic_responses_by_originator() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_RESPONSES, vec![("Originator", "codex_cli_rs")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("codex-cli"));
    }

    #[test]
    fn codex_cli_wins_over_generic_responses_by_ua() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_RESPONSES, vec![("User-Agent", "codex-tui/0.118.0")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("codex-cli"));
    }

    #[test]
    fn generic_responses_catches_no_codex_metadata() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_RESPONSES, vec![("User-Agent", "OpenAI/Python 1.50")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic-openai-responses"));
    }

    #[test]
    fn generic_chat_catches_openai_chat() {
        let reg = build_default_registry();
        let c = call_with(wa::OPENAI_CHAT, vec![("User-Agent", "OpenAI/JS 6.26.0")]);
        assert_eq!(reg.find(&c).map(|p| p.name()), Some("generic-openai-chat"));
    }
}
```

- [ ] **Step 4: Run** `cargo test -p ts-llm --lib agents::priority_tests` — expect: PASS (6/6).

- [ ] **Step 5: Run full ts-llm test suite to confirm nothing broke:** `cargo test -p ts-llm` — expect: all PASS.

- [ ] **Step 6: Commit**

```bash
git add server/ts-llm/src/agents/mod.rs
git commit -m "feat(agents): register generic-* profiles in default registry

Order is specific-first: claude-cli and codex-cli match by client header
before any generic profile gets a chance. Adds priority_tests module
that pins this contract — regressions there mean an existing profile's
traffic accidentally fell through to a generic catcher."
```

---

## Task 8: Counter Wiring (`processor.rs` + `stage.rs`)

**Files:**
- Modify: `server/ts-llm/src/processor.rs:167-184` (the `build_agent_call_info` function — extend signature)
- Modify: `server/ts-llm/src/processor.rs:147-152` (the call site inside `LlmProcessor::process`)
- Modify: `server/ts-llm/src/stage.rs:66-77` (the `register_worker` call — add the two new metrics)

**Context:** `build_agent_call_info` currently has no access to a counter handle. Two new counters need incrementing: (a) `LlmGenericToolIdCanonicalized` whenever `canonicalize_tool_id` actually changed its input, (b) `LlmGenericSessionIdUnsynth` when a generic-* profile matched but `extract_ids` returned `None`.

The cleanest path is to extend `build_agent_call_info` to take a `&MetricsWorker` parameter (it already lives at the llm-stage boundary; mirrors how `MetricsWorker` flows in other stages).

- [ ] **Step 1: Modify `canonicalize_tool_id` to report whether it changed input.** Update `generic_common.rs`:

```rust
/// Like canonicalize_tool_id but reports whether the input was modified.
/// The caller increments `LlmGenericToolIdCanonicalized` when changed.
pub fn canonicalize_tool_id_tracked(id: &str) -> (String, bool) {
    let canon = canonicalize_tool_id(id);
    let changed = canon != id;
    (canon, changed)
}
```

Update `compose_session_id` to use the tracked variant and return both the id and the change flag:

```rust
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
```

Add a unit test:

```rust
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
```

Run `cargo test -p ts-llm --lib agents::generic_common::tests::compose_tracks_canonicalization` — expect: PASS.

- [ ] **Step 2: Update each generic profile's `extract_ids`** to return the tracked tuple via a new internal field on `ExtractedIds` — but **simpler**: extend `ExtractedIds` itself.

In `server/ts-llm/src/profile.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedIds {
    pub session_id: String,
    /// True if the tool id used to derive `session_id` was modified by
    /// `canonicalize_tool_id`. Used by the llm stage to bump
    /// `LlmGenericToolIdCanonicalized`.
    pub tool_id_canonicalized: bool,
}
```

This is a public-API change. Update all profiles' `extract_ids`:

- `generic_anthropic.rs`, `generic_openai_chat.rs`, `generic_openai_responses.rs` — replace
  ```rust
  Some(ExtractedIds { session_id: compose_session_id(&user_text, sig) })
  ```
  with
  ```rust
  let (session_id, tool_id_canonicalized) = compose_session_id_tracked(&user_text, sig);
  Some(ExtractedIds { session_id, tool_id_canonicalized })
  ```
- `claude_cli.rs::extract_ids` — append `, tool_id_canonicalized: false` to its struct literal.
- `codex_cli.rs::extract_ids` — same.
- `profile.rs` test impl `FakeProfile::extract_ids` — same.

- [ ] **Step 3: Run** `cargo build -p ts-llm` — expect: clean compile (the field expansion is mechanical).

- [ ] **Step 4: Modify `build_agent_call_info`** in `processor.rs` to take a `MetricsWorker`:

```rust
pub fn build_agent_call_info(
    call: &LlmCall,
    registry: &AgentProfileRegistry,
    wire_apis: &WireApiRegistry,
    metrics: &ts_common::internal_metrics::MetricsWorker,
) -> Option<AgentCallInfo> {
    let profile = registry.find(call)?;
    let is_generic = profile.name().starts_with("generic-");
    let Some(ids) = profile.extract_ids(call) else {
        if is_generic {
            metrics.counter(ts_common::internal_metrics::Metric::LlmGenericSessionIdUnsynth).inc();
        }
        return None;
    };
    if ids.tool_id_canonicalized {
        metrics.counter(ts_common::internal_metrics::Metric::LlmGenericToolIdCanonicalized).inc();
    }
    Some(AgentCallInfo {
        agent_kind: profile.name(),
        session_id: ids.session_id,
        subagent_name: profile.subagent(call),
        is_user_turn_start: profile.is_user_turn_start(call),
        is_turn_terminal: profile.is_turn_terminal(call, wire_apis),
        is_auxiliary: profile.is_auxiliary(call),
        user_input: profile.extract_user_input(call),
        assistant_text: profile.extract_assistant_text(call),
    })
}
```

Update the call site at `processor.rs:154`:

```rust
    fn build_call_info(&self, call: &LlmCall) -> Option<AgentCallInfo> {
        build_agent_call_info(call, &self.registry, &self.wire_apis, &self.metrics)
    }
```

(`self.metrics` is the existing `MetricsWorker` field — confirm by reading `processor.rs:30-60` first; if the field name differs, use the actual one.)

- [ ] **Step 5: Update `processor.rs` tests** that call `build_agent_call_info` directly (`processor.rs:519, 533, 551, 572`) — pass the existing `test_metrics()` worker as the new arg. The test helper already exists.

- [ ] **Step 6: Update `test_metrics()` in `processor.rs:205-216`** to register the two new metrics:

```rust
    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[
                Metric::WireDetected,
                Metric::WireIgnored,
                Metric::LlmGenericToolIdCanonicalized,
                Metric::LlmGenericSessionIdUnsynth,
            ],
        );
        let _svc = sys.start();
        w
    }
```

- [ ] **Step 7: Update the production `register_worker` call** in `stage.rs` (lines ~66–77) to include the two new metrics. Find the existing list and append:

```rust
                Metric::LlmGenericToolIdCanonicalized,
                Metric::LlmGenericSessionIdUnsynth,
```

- [ ] **Step 8: Run** `cargo test -p ts-llm` — expect: all PASS.

- [ ] **Step 9: Run** `cargo build --workspace` — expect: clean build (no other crate breaks on the `ExtractedIds` struct change since the field is additive and we updated every constructor).

- [ ] **Step 10: Commit**

```bash
git add server/ts-llm/src/profile.rs server/ts-llm/src/agents/ server/ts-llm/src/processor.rs server/ts-llm/src/stage.rs
git commit -m "feat(ts-llm): wire generic-profile counters

ExtractedIds gains tool_id_canonicalized flag; build_agent_call_info
takes a MetricsWorker and increments LlmGenericToolIdCanonicalized when
the flag is true and LlmGenericSessionIdUnsynth when a generic-* profile
matched but extract_ids returned None. Both counters registered in the
llm stage worker."
```

---

## Task 9: ts-turn Integration Test

**Files:**
- Modify: `server/ts-turn/tests/integration.rs` (append a new test at the bottom)

**Context:** `ts-turn` already has end-to-end pcap-based tests. The new test needs to be lighter — it constructs `AgentCall` records directly via `build_agent_call_info` and feeds them to a `TurnTracker`, asserting the resulting `AgentTurn`. This avoids needing a new pcap fixture.

- [ ] **Step 1: Read the existing integration.rs** to learn the `AgentCall` construction pattern. Skim from line 1 to ~150 for helpers and one example test.

- [ ] **Step 2: Append** at the bottom of `server/ts-turn/tests/integration.rs`:

```rust
#[tokio::test]
async fn generic_anthropic_two_call_session() {
    use std::sync::Arc;
    use ts_common::internal_metrics::{Metric, MetricsSystem};
    use ts_llm::agents::build_default_registry;
    use ts_llm::build_agent_call_info;
    use ts_llm::model::{ApiType, LlmCall};
    use ts_turn::tracker::TurnTracker;
    use std::net::IpAddr;
    use std::time::Instant;

    fn make_call(req: &str, resp: &str, ts_us: i64, finish: Option<&str>) -> LlmCall {
        LlmCall {
            source_id: "test".into(),
            id: format!("c-{ts_us}"),
            wire_api: ts_llm::wire_apis::ANTHROPIC,
            model: "claude-3-5-sonnet".into(),
            api_type: ApiType::Chat,
            request_time: ts_us,
            response_time: Some(ts_us + 10_000),
            complete_time: Some(ts_us + 50_000),
            request_path: "/v1/messages".into(),
            is_stream: true,
            request_body: Some(req.into()),
            status_code: Some(200),
            finish_reason: finish.map(str::to_string),
            response_body: Some(resp.into()),
            input_tokens: Some(10),
            output_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: Some(50),
            e2e_latency_ms: Some(100),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 4444,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 443,
            response_id: None,
            // No claude-cli UA — falls to generic-anthropic.
            request_headers: vec![("User-Agent".into(), "anthropic/0.40 python/3.12".into())],
            response_headers: vec![],
        }
    }

    let mut sys = MetricsSystem::new();
    let metrics = sys.register_worker(
        "test",
        &[
            Metric::WireDetected,
            Metric::WireIgnored,
            Metric::LlmGenericToolIdCanonicalized,
            Metric::LlmGenericSessionIdUnsynth,
            Metric::TurnsCompleted,
            Metric::TurnCallsIngested,
            Metric::TurnCallsAuxiliary,
            Metric::TurnCallsDroppedLate,
            Metric::TurnClosedByGrace,
            Metric::TurnClosedByIdle,
            Metric::TurnDiscardedNoUserStart,
            Metric::TurnActive,
        ],
    );
    let _svc = sys.start();

    let registry = Arc::new(build_default_registry());
    let wire_apis = Arc::new(ts_llm::wire_apis::build_default_wire_api_registry());

    // Call #1: user prompt → assistant tool_use (not terminal).
    let req1 = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"fix the bug"}]}]}"#;
    let resp1 = r#"{"content":[{"type":"tool_use","id":"toolu_pcap","name":"Read","input":{"path":"/x"}}],"stop_reason":"tool_use"}"#;
    let call1 = make_call(req1, resp1, 1_000_000, Some("tool_use"));
    let info1 = build_agent_call_info(&call1, &registry, &wire_apis, &metrics).expect("info1");
    assert_eq!(info1.agent_kind, "generic-anthropic");
    assert_eq!(info1.session_id, "toolu_pcap");
    assert_eq!(info1.is_user_turn_start, Some(true));
    assert!(!info1.is_turn_terminal, "tool_use is not terminal");

    // Call #2: tool_result → assistant text (terminal end_turn).
    let req2 = r#"{"messages":[
        {"role":"user","content":[{"type":"text","text":"fix the bug"}]},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_pcap","name":"Read","input":{"path":"/x"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_pcap","content":"file ok"}]}
    ]}"#;
    let resp2 = r#"{"content":[{"type":"text","text":"the bug is at line 42"}],"stop_reason":"end_turn"}"#;
    let call2 = make_call(req2, resp2, 2_000_000, Some("end_turn"));
    let info2 = build_agent_call_info(&call2, &registry, &wire_apis, &metrics).expect("info2");
    assert_eq!(info2.session_id, "toolu_pcap", "call #2 must hit same session as call #1");
    assert!(info2.is_turn_terminal, "end_turn is terminal");

    // Feed both into the tracker and force-flush.
    let mut tracker = TurnTracker::new(ts_turn::tracker::TrackerConfig::default(), metrics.clone());
    let mut events = Vec::new();
    let now = Instant::now();
    events.extend(tracker.ingest(ts_turn::AgentCall { call: Arc::new(call1), agent: info1 }, now));
    events.extend(tracker.ingest(ts_turn::AgentCall { call: Arc::new(call2), agent: info2 }, now));
    // Force grace expiry by advancing wall clock past the grace window.
    let later = now + std::time::Duration::from_secs(2);
    events.extend(tracker.flush_ready(later));

    let turns: Vec<_> = events
        .into_iter()
        .filter_map(|e| match e {
            ts_turn::TurnEvent::Completed(t) => Some(t),
        })
        .collect();
    assert_eq!(turns.len(), 1, "exactly one turn");
    let t = &turns[0];
    assert_eq!(t.session_id, "toolu_pcap");
    assert_eq!(t.agent_kind, "generic-anthropic");
    assert_eq!(t.call_count, 2);
    assert_eq!(t.user_input_preview.as_deref(), Some("fix the bug"));
    assert_eq!(t.final_answer_preview.as_deref(), Some("the bug is at line 42"));
    assert!(matches!(t.status, ts_turn::TurnStatus::Complete));
}
```

**Note:** the `AgentCall` constructor and `TurnTracker::flush_ready` / `tracker::TrackerConfig::default()` / `TurnEvent::Completed` symbols may not be exposed in this exact form. Before running, grep `ts-turn/src/lib.rs` and `ts-turn/src/tracker.rs` for the actual public API and adjust:

```bash
grep -E "pub (fn|struct|enum) " server/ts-turn/src/lib.rs server/ts-turn/src/tracker.rs server/ts-turn/src/model.rs
```

If the existing tests use different helper names (e.g., `TurnTracker::ingest_call`, `tick_now`, etc.), follow that style — the key invariants the test must verify are unchanged: (1) two calls produce one `AgentTurn`, (2) `session_id == "toolu_pcap"`, (3) `agent_kind == "generic-anthropic"`, (4) `user_input_preview` and `final_answer_preview` populated.

- [ ] **Step 3: Run** `cargo test -p ts-turn --test integration generic_anthropic_two_call_session` — expect: PASS.

- [ ] **Step 4: Run full workspace tests** to confirm nothing broke: `cargo test --workspace` — expect: all PASS.

- [ ] **Step 5: Commit**

```bash
git add server/ts-turn/tests/integration.rs
git commit -m "test(ts-turn): cover generic-anthropic two-call session

Constructs two LlmCall records (no claude-cli UA), runs them through
build_agent_call_info + TurnTracker, asserts a single AgentTurn with
session_id matching the synthesized canonical tool_use id."
```

---

## Task 10: Smoke Test Against Real Pcap (Manual Verification)

**Files:**
- None (manual verification step)

**Context:** The validation done during brainstorming used a Python re-implementation. Now we have the real Rust code; one smoke run against `~/Downloads/openclaw-multi-sessions.pcap` proves the e2e pipeline produces the expected 2 sessions instead of 4.

- [ ] **Step 1: Build the binary**

```bash
cargo build --release -p tokenscope
```

- [ ] **Step 2: Run a one-shot pcap ingest** (consult `server/app/tokenscope/` or `justfile` for the actual invocation; commonly `tokenscope --pcap <file>`):

```bash
./target/release/tokenscope --pcap ~/Downloads/openclaw-multi-sessions.pcap --config server/config/default.toml --once
```

If a `--once` flag does not exist, run normally and Ctrl-C after pcap completes (`flush_all` should emit final turns).

- [ ] **Step 3: Inspect emitted AgentTurns.** Either via the SQLite/DuckDB store the binary produces or via stdout logs. Verify:
  - Exactly **2 turns** with `agent_kind == "generic-openai-chat"`
  - Their `session_id`s are `call_50e1891408d545d598f7c6cc` and `call_d9c1e9e6617a41ca860562a1` (canonical, with underscore)
  - Counter `worker::llm::generic_tool_id_canon` shows ≥ 2 (at minimum each session's call #1 underwent canonicalization; `extract_ids` is also called for follow-up calls but those echo without underscore so ALSO canonicalize — expect ≥ 17)
  - Counter `worker::llm::generic_session_unsynth` is 0 (every call should bind)

- [ ] **Step 4: Optional sanity** — same run against `~/Downloads/codex-cli-messages-multi.pcap`. Expected: still produces exactly 1 codex-cli turn (specific profile wins; generic-openai-responses never matches). Also: `generic_tool_id_canon` increments are 0 (codex preserves underscores).

- [ ] **Step 5: Same run against `~/Downloads/claude-cli-messages-multi.pcap`.** Expected: claude-cli turns unchanged (specific profile wins). `generic_tool_id_canon` 0.

- [ ] **Step 6: No commit needed for this task** — it's verification. If the run reveals discrepancies, file follow-up issues / fix them as needed before declaring the feature done.

---

## Self-Review Notes

**Spec coverage:** Each spec section maps to:
- "File Layout" → Tasks 3–7 (file creation), Task 2 (helper migration)
- "Registry Order = Priority" → Task 7
- "Common Skeleton" + per-profile differences → Tasks 4, 5, 6
- "Tool-id Canonicalization" → Task 3
- "Codex Helper Migration" → Task 2
- "Edge Cases & Failure Modes" → covered inside per-profile test suites (Tasks 4–6)
- "Tests" → Tasks 4–7 (unit), Task 9 (integration)
- "Counters Added" → Tasks 1 (definition), 8 (wiring)
- "Verification Artifacts" → Task 10

**No-placeholder check:** Every code block has the actual code. The one place where the engineer needs to look at existing code to confirm names is Task 9 step 2 (`AgentCall` / `TurnTracker` API) — the note explicitly tells them to grep first; the test invariants are concrete.

**Type consistency:** `ExtractedIds` gains `tool_id_canonicalized: bool` in Task 8 and every call site (3 generic profiles + claude_cli + codex_cli + the FakeProfile in tests) sets the field consistently. After Task 8 step 2, the original `compose_session_id` from Task 3 becomes dead code — the engineer should delete it (and `canonicalize_tool_id_tracked` if it ends up wrapping unused) when running the workspace build at step 9; `cargo build` will emit a `dead_code` warning that pinpoints the exact lines.

---

Plan complete and saved to `docs/superpowers/plans/2026-04-28-generic-agent-profile.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
