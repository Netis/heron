# Generic Agent Profiles for Header-less LLM Traffic

## Context

Today `ts-turn` participates only in traffic matched by `claude-cli` or `codex-cli` profiles. Both rely on client-supplied identifiers — `X-Claude-Code-Session-Id` header for Anthropic; body fields `session_id` / `turn_id` for OpenAI Responses. Any traffic without those markers — Python scripts using `openai` / `anthropic` SDKs, the OpenClaw TUI, generic curl harnesses, third-party agent frameworks — falls into `(source_id, "")` and never produces an `AgentTurn`.

This is a hard cap on TokenScope's coverage. Most LLM agent deployments in the wild do not emit Claude-Code-style headers.

This spec adds three new `AgentProfile` impls that synthesize a `session_id` from the request/response payload alone. They sit below `claude-cli` / `codex-cli` in the registry so existing matched traffic is unaffected.

## Goals

- Produce stable `AgentTurn`s for header-less LLM traffic on three wire APIs: Anthropic Messages, OpenAI Chat Completions, OpenAI Responses.
- `session_id` is a pure function of the `LlmCall` body — no cross-call state in `ts-llm`.
- Same synthesis result whether computed at call #1 (from `response_body`) or call #N (from `messages` history) — equality holds even when the client normalizes some fields between server emission and history echo.
- `ts-turn` mechanism (buffer / grace / finalize / discard) unchanged — only widens the set of `(source, session_id)` buckets it sees.

## Non-goals

- Distinguishing main-agent vs sub-agent vs auxiliary calls. Those classifications are profile-specific (claude-cli reads the `tools` array; the user has no equivalent generic signal).
- Conversation-compaction recovery. When a client truncates `messages[0]` into a summary, the synthesized session may split. Counter-instrumented but not auto-merged in v1. See *Future Evolution*.
- Sub-agent dispatching modeled as nested sessions. A generic agent that does sub-agent style dispatches will surface them as independent sessions — that is the structural truth absent profile-specific knowledge.

## Approach

### File Layout

```
server/ts-llm/src/agents/
├── claude_cli.rs                      (unchanged)
├── codex_cli.rs                       (slim: pull body_has_terminal_message_only out)
├── generic_anthropic.rs               (new)
├── generic_openai_chat.rs             (new)
├── generic_openai_responses.rs        (new)
├── generic_common.rs                  (new — canonicalize_tool_id helper)
└── mod.rs                              (registry: 3 new with(...) lines)

server/ts-llm/src/wire_apis/
└── openai_responses.rs                (gains pub body_has_terminal_message_only)
```

### Registry Order = Priority

```rust
AgentProfileRegistry::new()
    .with(Box::new(claude_cli::ClaudeCliProfile))
    .with(Box::new(codex_cli::CodexCliProfile))
    .with(Box::new(generic_anthropic::GenericAnthropicProfile))
    .with(Box::new(generic_openai_chat::GenericOpenAiChatProfile))
    .with(Box::new(generic_openai_responses::GenericOpenAiResponsesProfile))
```

`AgentProfileRegistry::find_for(call)` is first-match-wins. Traffic carrying `claude-cli/` UA or `X-Codex-Turn-Metadata` continues to match the specific profiles; everything else falls to one of the three generics.

### `ts-turn` Stays Untouched

`AgentCallInfo.session_id` carries the synthesized id exactly the same way it carries header-extracted ids today. The buffer key `(source_id, session_id)`, the grace timer, the `is_user_turn_start` discard rule — all unchanged. The tracker remains profile-agnostic.

## Profile Design

### Common Skeleton

All three generic profiles share:

```rust
fn name(&self) -> &'static str { /* "generic-anthropic" / "generic-openai-chat" / "generic-openai-responses" */ }
fn subagent(&self, _: &LlmCall) -> Option<String> { None }
fn is_auxiliary(&self, _: &LlmCall) -> bool { false }
```

`extract_ids` shape is identical across profiles; only the inner `extract_first_user_text` and `extract_first_assistant_signature` differ:

```rust
fn extract_ids(&self, call: &LlmCall) -> Option<ExtractedIds> {
    let user_text = self.first_user_text(call.request_body.as_deref()?)?;
    let sig = self.first_assistant_sig_from_request(call.request_body.as_deref()?)
        .or_else(|| self.first_assistant_sig_from_response(call.response_body.as_deref()?))?;

    let session_id = match sig {
        AssistantSig::ToolId(id) => canonicalize_tool_id(&id),
        AssistantSig::Text(text) => format!("gen-{:016x}", fast_hash(&user_text, &text)),
    };
    Some(ExtractedIds { session_id })
}
```

`AssistantSig` is a profile-internal enum, not exposed in the trait. Each profile parses its own JSON shape — accepting some duplication over a forced abstraction.

`fast_hash` is non-crypto (`fxhash` or `seahash`), 64-bit, hex-formatted. Crypto strength is not required; we only need stable, fast, low-collision.

### Per-profile Differences

| Field | generic-anthropic | generic-openai-chat | generic-openai-responses |
|---|---|---|---|
| `matches` | `wire_api == ANTHROPIC` | `wire_api == OPENAI_CHAT` | `wire_api == OPENAI_RESPONSES` |
| First user text — request body | `messages[0..].first(role=user).content[].type=="text".text` joined (or `content` if string) | `messages[0..].first(role=user).content` (string or `content[].text` joined) | `input[].first(type=message, role=user).content` |
| First assistant signature — request body | `messages[..].first(role=assistant).content[].type=="tool_use".id`; else `content[].type=="text".text` joined | `messages[..].first(role=assistant).tool_calls[0].id`; else `content` | `input[].first(type=function_call).call_id`; else `input[].first(type=message, role=assistant).content[].text` |
| First assistant signature — response body | `response_body.content[].type=="tool_use".id` (parsed) or `text` joined | `response_body.choices[0].message.tool_calls[0].id`; else `.content` | `response_body.output[].type=="function_call".call_id`; else `.output[].type=="message".content[].text` |
| `extract_user_input` | last `role=user` text blocks joined (no system-reminder stripping) | last `role=user` content | `input[]` reverse-walk for `type=message, role=user` |
| `extract_assistant_text` | `response_body.content[].text` joined | `response_body.choices[0].message.content` | `response_body.output[]` first message text |
| `is_turn_terminal` | trait default (`wire-api.is_terminal && !is_tool_use`) | trait default | **override** → `wire_apis::openai_responses::body_has_terminal_message_only` |
| `is_user_turn_start` reject types | `tool_result` content blocks | role != user, or content all tool-result-like | `function_call_output` items |

### Tool-id Canonicalization

OpenClaw evidence (see *Verification Artifacts*) shows real-world clients normalizing the tool id between server emission and history echo. Specifically:

| Source | Value |
|---|---|
| LLM response stream (server emission) | `call_d9c1e9e6617a41ca860562a1` |
| OpenClaw history echo (`messages[k].tool_calls[0].id`) | `calld9c1e9e6617a41ca860562a1` (underscore stripped) |

Without canonicalization, the *same* tool id appears as two distinct strings → call #1 (synthesized from response) lands in a different bucket from call #2+ (synthesized from messages history) → first call of every session orphaned.

The fix is to canonicalize the tool id back to its server-emitted form before using it as `session_id`:

```rust
// generic_common.rs
const PREFIXES: &[&str] = &["call", "toolu", "fc", "chatcmpl"];

pub fn canonicalize_tool_id(id: &str) -> String {
    for p in PREFIXES {
        let Some(after) = id.strip_prefix(p) else { continue };
        if !after.is_empty() && !after.starts_with('_') {
            return format!("{p}_{after}");
        }
    }
    id.to_string()
}
```

Rule: if a tool id starts with one of the well-known LLM-side prefixes (`call_`, `toolu_`, `fc_`, `chatcmpl_`) but the underscore is missing, restore it. Canonical form = LLM-side form; debugging stays grep-able against capture data.

Future client quirks (lowercase normalization, prefix swap, truncation) are not handled by v1. Each new normalization is a small targeted patch to this function — its surface area is intentional.

### Codex Helper Migration

`body_has_terminal_message_only` (today private in `ts-llm/src/agents/codex_cli.rs`) moves to `ts-llm/src/wire_apis/openai_responses.rs` as `pub`. Both `CodexCliProfile::is_turn_terminal` and `GenericOpenAiResponsesProfile::is_turn_terminal` become single-line calls into it.

Function shape stays:

```rust
pub fn body_has_terminal_message_only(response_body: Option<&str>) -> bool { ... }
```

Existing `codex_cli` tests for this function move to `wire_apis::openai_responses`. `codex_cli`'s `is_turn_terminal` test reduces to a thin "delegates to helper" check.

## Edge Cases & Failure Modes

| # | Case | Handling | Counter |
|---|---|---|---|
| C1 | Conversation compaction (`messages[0]` rewritten into summary) | session_id may split; the tool-id-preferred path survives compaction (`messages[k].tool_calls[0].id` is preserved across compaction in observed OpenClaw data); only text-fallback paths are vulnerable | `worker::generic::session_compacted_split` (future) |
| C2 | Real first-call (`messages.len()==1`) AND `response_body` missing or malformed | `extract_ids` returns `None` → call lands in `(source_id, "")` bucket (current fallback) | `worker::generic::session_id_unsynth` |
| C3 | Single-call session (call #1 already terminal, no follow-up) | Synthesized from response body, terminal verdict from finish_reason; closes via grace as a 1-call turn | reuses `TurnClosedByGrace` |
| C4 | Anthropic `cache_control` markers in content blocks | Filtered by `type=="text"` check during extraction; ignored | — |
| C5 | OpenClaw-style tool-id normalization (underscore stripped between response and echo) | `canonicalize_tool_id` re-inserts the missing underscore; both ends produce identical canonical id | `worker::generic::tool_id_canonicalized` (counts insertions) |
| C6 | Two parallel sessions on same source with identical first user text | Distinguished by tool id (server-generated UUID); never collides | — |
| C7 | First assistant has only `text` (no tool); two parallel sessions emit identical text | Hash collision possible but rare — LLM responses vary even at low temperature; accepted | — |
| C8 | OpenAI Chat `messages[0]` is `role=system` | First user text walker scans for first `role=user`, not `messages[0]` | — |
| C9 | OpenAI Responses `input` is a string (simplified mode) instead of array | Falls through to response-body fallback (no assistant in history); single-call session handled via C3 | — |
| C10 | Real first call AND response has neither tool nor text (only thinking / empty) | `sig` is None → C2 path | counted with `session_id_unsynth` |

C2 / C10 trigger only on (a) a genuinely first call of a session AND (b) response capture failed or response was content-empty. Late-start capture is *not* a failure mode: every subsequent call carries full history and produces identical synthesized id.

### Resolved Concerns from Validation

The validation pass (see *Verification Artifacts*) ruled out two earlier worries:

- **"Late-start capture loses sessions."** False — `messages` history echoes the original first user message and first assistant signature on every subsequent call. Joining mid-stream produces the same synthesized id as joining from call #1.
- **"Compaction breaks session continuity."** Partial — compaction does rewrite `messages[0]`, but the preserved `messages[k].tool_calls[0].id` keeps the tool-id-preferred path stable. Text-fallback path remains vulnerable; tracked via C1.

## Tests

### Unit Tests Per Profile (~10 each, ~30 total)

Each generic profile has a `#[cfg(test)] mod tests` with these cases (test names suggest, not enforce, structure):

| Test | Input | Expected |
|---|---|---|
| `matches_yes` | matching `wire_api`, no client headers | `true` |
| `matches_no_other_wire_api` | other `wire_api` | `false` |
| `extract_ids_call_n_with_tool_history` | `messages` includes assistant with tool | `session_id == canonicalize(tool_id)` (no `gen-` prefix) |
| `extract_ids_call_n_with_text_only_history` | assistant has only text | `session_id` matches `gen-<16hex>` shape |
| `extract_ids_call_1_tool_in_response` | `messages.len()==1`, `response_body` has tool | `session_id == canonicalize(tool_id)` from response |
| `extract_ids_call_1_text_in_response` | `messages.len()==1`, `response_body` text only | `session_id` matches `gen-<16hex>` |
| `extract_ids_call_1_and_n_match` | construct call #1 (one user msg + response) and call #2 (history echo), feed both | both `extract_ids` return identical `session_id` |
| `extract_ids_call_1_with_normalized_tool_id` | response tool id has underscore, echoed without (or vice versa) | both forms canonicalize to identical id |
| `extract_ids_none_when_first_call_no_response` | `messages.len()==1`, no response | `None` |
| `extract_ids_none_when_malformed_json` | garbage body | `None` |
| `is_user_turn_start_text` | last user message has text content | `Some(true)` |
| `is_user_turn_start_tool_result_only` | last user message is tool result only | `Some(false)` |
| `is_turn_terminal_default_finish_reason` (chat / anthropic) | finish_reason variants | maps via wire-api default |
| `is_turn_terminal_via_responses_helper` (responses only) | output items terminal vs pending | delegates to helper |

The most load-bearing case is `extract_ids_call_1_and_n_match` — it codifies the "no cross-call state, equal under any client normalization" promise.

### `canonicalize_tool_id` Unit Tests

Co-located with the helper:

| Test | Input | Output |
|---|---|---|
| `passthrough_underscore_present` | `"call_abc"` | `"call_abc"` |
| `inserts_for_call_prefix` | `"calld9c1..."` | `"call_d9c1..."` |
| `inserts_for_toolu_prefix` | `"tooluxyz"` | `"toolu_xyz"` |
| `inserts_for_fc_prefix` | `"fcabc"` | `"fc_abc"` |
| `inserts_for_chatcmpl_prefix` | `"chatcmplabc"` | `"chatcmpl_abc"` |
| `passthrough_unknown_prefix` | `"abc_xyz"` | `"abc_xyz"` (unchanged) |
| `passthrough_empty_after_prefix` | `"call"` | `"call"` (degenerate, not modified) |

### Registry Priority Tests

In `agents/mod.rs` (or new `profile_priority.rs` test module):

| Test | Input | Expected `find_for` result |
|---|---|---|
| `claude_cli_wins_over_generic_anthropic` | `wire_api=anthropic` + UA `claude-cli/2.1.98` | `ClaudeCliProfile` |
| `generic_anthropic_catches_no_ua_anthropic` | `wire_api=anthropic`, no UA | `GenericAnthropicProfile` |
| `codex_cli_wins_over_generic_responses` | `wire_api=openai-responses` + `X-Codex-Turn-Metadata` | `CodexCliProfile` |
| `generic_responses_catches_no_codex_metadata` | `wire_api=openai-responses` no codex header | `GenericOpenAiResponsesProfile` |
| `generic_chat_catches_openai_chat` | `wire_api=openai-chat` | `GenericOpenAiChatProfile` |

### `ts-turn` Integration Test (1 new)

In `server/ts-turn/tests/integration.rs`. Synthesize a generic-anthropic call sequence (no session header):

- `[user text → assistant tool_use] [tool_result → assistant text (end_turn)]`
- Assert: 1 `AgentTurn` emitted
- Assert: `session_id == "call_<original tool_use_id>"` (canonical)
- Assert: `agent_kind == "generic-anthropic"`
- Assert: `user_input_preview` from first user, `final_answer_preview` from final assistant text

The test does not re-cover buffer / grace / idle — those are already exercised by existing tracker tests. It only verifies the new profile produces an `AgentCallInfo` shape that the unchanged tracker assembles correctly.

### Codex Helper Migration Tests

The existing `codex_cli` tests for `body_has_terminal_message_only` move with the function to `wire_apis::openai_responses`. `codex_cli`'s remaining `is_turn_terminal` test reduces to "calls helper, returns its result."

## Counters Added

Three new internal metrics, all in the `worker::generic::` family:

| Counter | Where | When |
|---|---|---|
| `worker::generic::tool_id_canonicalized` | `canonicalize_tool_id` (only when it actually changed input) | Each call whose tool id needed underscore reinsertion |
| `worker::generic::session_id_unsynth` | `extract_ids` returning `None` | First-call without response, malformed JSON, no extractable signature |
| `worker::generic::session_compacted_split` (deferred to v2) | future detector | When a generic session shows summary-style `messages[0]` rewrite |

These follow the existing `worker::*` naming convention. The first two ship in v1; the third is a placeholder until we observe the failure rate in production.

## Verification Artifacts

Three real-world packet captures validated the design pre-implementation. Each was analyzed by re-running the proposed algorithm by hand on extracted JSON bodies.

| Pcap | Client | Wire API | Expected sessions | Algorithm result |
|---|---|---|---|---|
| `~/Downloads/codex-cli-messages-multi.pcap` | `codex-tui/0.118.0` | OpenAI Responses | 1 | 1 (40 calls, all bound including call #0) |
| `~/Downloads/claude-cli-messages-multi.pcap` | `claude-cli/2.1.104` | Anthropic | 2 main + N aux | 2 main + 1 aux singletons (matches generic semantics — claude-specific aux/sub-agent classification not attempted) |
| `~/Downloads/openclaw-multi-sessions.pcap` | OpenAI/JS + GLM-5 | OpenAI Chat | 2 | 2 (after canonicalization); 4 (without — each session orphans its first call) |

OpenClaw is the **key driving sample**: it exhibits two real client-side normalizations:

1. Tool id: `call_<hex>` → `call<hex>` (underscore stripped between response stream and history echo). Resolved by `canonicalize_tool_id`.
2. Function arguments: `{"command": "x"}` → `{"command":"x"}` (whitespace removed). Not on the synthesis path; noted as a class-of-quirk indicator.

Both sessions in OpenClaw also exhibited mid-session conversation compaction (`messages[0]` shrunk from 198 to 46 chars between call #6 and call #9). The tool-id-preferred path survived this — `messages[k].tool_calls[0].id` is preserved across compaction.

The validation script and intermediate JSON dumps live at `/tmp/pcap-validation/` (not checked in; pcaps are user-local). Re-running:

```bash
# Extract bodies
tshark -r <pcap> -2 -Y "http.request.method == POST" \
    -o http.desegment_body:TRUE -o tcp.desegment_tcp_streams:TRUE \
    -T json -j "frame http json" > requests.json

# Apply algorithm: see /tmp/pcap-validation/full.py for the reference impl
```

## Future Evolution

If production counters show:

- High `tool_id_canonicalized` rate AND post-canonical session orphans → a client is doing a normalization beyond underscore handling. Extend `canonicalize_tool_id` with a new rule (lowercase, prefix swap, etc.) — small targeted patch.
- `session_compacted_split` becomes meaningful → consider a dual-key buffer index in `ts-turn`: each `SessionBuffer` registers under (a) `canonicalized_tool_id` (primary, survives most normalizations), and (b) `hash(user_text + first_assistant_function_name)` (secondary, survives compaction when the first tool call is preserved). New calls match either anchor. This is a `ts-turn` change; deferred until evidence justifies the complexity.

## Out of Scope

- Cross-source session merging (multi-NIC capturing the same session). Same as today.
- Streaming-end-of-message detection beyond what `wire_apis` already provides.
- Compaction-aware re-binding (see Future Evolution).
- Sub-agent / auxiliary classification for generic profiles. Profile-specific signals don't generalize.
