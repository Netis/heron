# LLM Call Detail — Structured IO Redesign — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **Project rule: do not run `git commit` without the user's explicit go-ahead.** Stage changes with `git add`, report what's staged, and wait for the user to say "commit" before running `git commit`.

**Goal:** Reframe `/llm-calls` detail as a single-call structured IO view — parsed `messages[]`, `system`, `tools[]`, `sampling` on the input side, reuse the existing parsed `reasoning`/`message`/`tool_calls` on the output side, and move raw HTTP behind a single link into the existing `RawHttpDrawer`.

**Architecture:** Extend `ts_llm::model::ParsedInput` with new fields, populate them in each `WireApi::parse_input` implementation, expose them via `/api/llm-calls/{id}` (`EnrichedCallDetail.parsed_input`), and rebuild `LlmCallDetailPanel` from focused components under `console/src/components/llm-call-detail/`. Extract the existing Output rendering into a shared component so Turn Detail's `CallCard` and the new panel both consume it.

**Tech Stack:** Rust (Tokio / Axum / serde) for backend; React 19 + TypeScript + Tailwind v4 for the console. `cargo test` for backend verification, `bun run lint` + `tsc -b` for frontend typecheck, manual browser verification for UI. No frontend test framework is installed; component tests are out of scope for this plan.

**Spec:** `docs/superpowers/specs/2026-04-22-llm-call-detail-structured-design.md`

---

## File Structure

**Backend — modified**

- `server/ts-llm/src/model.rs` — extend `ParsedInput` with `messages` / `system` / `tools` / `sampling`; add `ParsedMessage`, `ParsedRole`, `ParsedContentBlock`, `ParsedToolDef`, `ParsedSampling`; add serde attributes for wire-format serialization.
- `server/ts-llm/src/wire_apis/anthropic.rs` — extend `parse_input` to fill the new fields.
- `server/ts-llm/src/wire_apis/openai.rs` — extend `parse_input` for both `OpenAiChatWireApi` and `OpenAiResponsesWireApi`.
- `server/ts-api/src/routes/turn_call_enrichment.rs` — add `parsed_input: ParsedInput` to `EnrichedCallDetail`; populate in `enrich_single`.

**Backend — new fixtures**

- `server/ts-llm/tests/fixtures/anthropic_input_full.json` — rich Anthropic request covering `system`, multi-role messages with `tool_use` + `tool_result`, `tools[]`, and sampling fields.
- `server/ts-llm/tests/fixtures/openai_chat_input_full.json` — Chat request with `system` in `messages[0]`, `tools[]`, sampling.
- `server/ts-llm/tests/fixtures/openai_responses_input_full.json` — Responses request with `instructions`, `input[]`, `tools[]`, sampling.

**Frontend — modified**

- `console/src/types/api.ts` — add `ParsedRole`, `ParsedContentBlock`, `ParsedMessage`, `ParsedToolDef`, `ParsedSampling`, `ParsedInput`; add `parsed_input` to `LlmCallDetail`.
- `console/src/components/turn-detail/call-card.tsx` — replace its inner output JSX with `<CallParsedOutput />`.
- `console/src/pages/llm-call-detail-panel.tsx` — rewritten to compose the new sections.

**Frontend — new**

- `console/src/components/call-parsed-output.tsx` — shared Reasoning / Message / Tool calls renderer (extracted from CallCard).
- `console/src/components/llm-call-detail/summary-cards.tsx` — four-card summary (lifted).
- `console/src/components/llm-call-detail/timeline-bar.tsx` — TTFB/Gen bar (lifted).
- `console/src/components/llm-call-detail/metadata-grid.tsx` — compact metadata grid (lifted).
- `console/src/components/llm-call-detail/sampling-block.tsx` — single-line sampling params.
- `console/src/components/llm-call-detail/tools-block.tsx` — collapsible tools list.
- `console/src/components/llm-call-detail/system-block.tsx` — collapsible top-level system prompt.
- `console/src/components/llm-call-detail/messages-block.tsx` — per-role message list with content expansion.
- `console/src/components/llm-call-detail/input-section.tsx` — container that composes the four input blocks.

---

## Task 1: Extend `ParsedInput` with new fields (no parser changes)

**Files:**
- Modify: `server/ts-llm/src/model.rs`

- [ ] **Step 1: Add the new types and fields**

Edit `server/ts-llm/src/model.rs`. Add `serde::{Deserialize, Serialize}` imports at the top (serde is already a workspace dependency; confirm with `grep '^serde' server/Cargo.toml` — if the crate doesn't have it yet, add `serde = { workspace = true, features = ["derive"] }` to `server/ts-llm/Cargo.toml`).

Replace the existing `ParsedInput` struct and tool-related types with:

```rust
/// Structured view of an LLM input extracted from a request body.
/// Per-wire-api implementations of `WireApi::parse_input` produce this.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ParsedInput {
    /// Most-recent user message in the input, if any. Consumed by turn joiner.
    pub user_message: Option<String>,
    /// Tool results keyed by the `id` / `call_id` they belong to. Consumed by turn joiner.
    pub tool_results: Vec<ParsedToolResult>,

    /// Full ordered message history. Empty when the parser couldn't read `messages[]`.
    pub messages: Vec<ParsedMessage>,
    /// Top-level system prompt (Anthropic `system` field, OpenAI Responses `instructions`).
    /// `None` for wire APIs where system lives inside `messages[]`.
    pub system: Option<String>,
    /// Tool definitions declared in the request.
    pub tools: Vec<ParsedToolDef>,
    /// Sampling / control parameters.
    pub sampling: ParsedSampling,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ParsedMessage {
    pub role: ParsedRole,
    pub content: Vec<ParsedContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParsedRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParsedContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        args_json: String,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    Image {
        mime: Option<String>,
        size_bytes: Option<u64>,
    },
    /// Forward-compat: unknown content block types are preserved as raw JSON
    /// rather than dropped, so the frontend can render them as
    /// `⚠️ unknown block: {type}` without silently losing payload data.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ParsedToolDef {
    pub name: String,
    pub description: Option<String>,
    /// Raw JSON string of the tool's `input_schema` / `parameters`. The frontend
    /// pretty-prints; we keep it as a string to avoid a round-trip through
    /// `Value` on every render.
    pub input_schema_json: String,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ParsedSampling {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub stream: Option<bool>,
    /// Serialized back to a string — may be plain `"auto"`, `"any"`, or a JSON object.
    pub tool_choice: Option<String>,
    pub stop: Vec<String>,
    /// Serialized back to a JSON string when non-trivial (e.g. `{"type":"json_schema", ...}`).
    pub response_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ParsedToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}
```

Note: the existing `ParsedToolCall` stays unchanged (output-side). The existing `ParsedToolResult` definition is removed from its current location and replaced by the one above with `#[derive(..., serde::Serialize)]`.

- [ ] **Step 2: Write a serialization test**

Add to the existing `mod extension_tests` block in `server/ts-llm/src/model.rs`:

```rust
#[test]
fn parsed_content_block_serializes_with_type_tag() {
    let t = ParsedContentBlock::Text { text: "hi".into() };
    let tu = ParsedContentBlock::ToolUse {
        id: "id1".into(),
        name: "read".into(),
        args_json: "{}".into(),
    };
    let tr = ParsedContentBlock::ToolResult {
        tool_use_id: "id1".into(),
        content: "ok".into(),
        is_error: false,
    };
    assert_eq!(
        serde_json::to_value(&t).unwrap(),
        serde_json::json!({"type":"text","text":"hi"}),
    );
    assert_eq!(
        serde_json::to_value(&tu).unwrap(),
        serde_json::json!({"type":"tool_use","id":"id1","name":"read","args_json":"{}"}),
    );
    assert_eq!(
        serde_json::to_value(&tr).unwrap(),
        serde_json::json!({"type":"tool_result","tool_use_id":"id1","content":"ok","is_error":false}),
    );
}

#[test]
fn parsed_content_block_unknown_preserves_raw() {
    let raw = serde_json::json!({"type":"future_kind","mystery":42});
    let block = ParsedContentBlock::Unknown(raw.clone());
    assert_eq!(serde_json::to_value(&block).unwrap(), raw);
}

#[test]
fn parsed_role_serializes_lowercase() {
    assert_eq!(serde_json::to_value(ParsedRole::System).unwrap(), "system");
    assert_eq!(serde_json::to_value(ParsedRole::User).unwrap(), "user");
    assert_eq!(
        serde_json::to_value(ParsedRole::Assistant).unwrap(),
        "assistant",
    );
    assert_eq!(serde_json::to_value(ParsedRole::Tool).unwrap(), "tool");
}

#[test]
fn parsed_input_default_has_empty_new_fields() {
    let p = ParsedInput::default();
    assert!(p.messages.is_empty());
    assert!(p.system.is_none());
    assert!(p.tools.is_empty());
    assert_eq!(p.sampling, ParsedSampling::default());
}
```

- [ ] **Step 3: Verify the crate still builds and existing tests pass**

Run:

```
cd server && cargo test -p ts-llm --lib
```

Expected: all existing tests pass (they don't reference the new fields). The four new tests above pass.

Run the workspace check:

```
cd server && cargo check
```

Expected: zero errors. The two existing `parse_input` call sites in `turn_call_enrichment.rs` still compile because they only read `user_message` / `tool_results`.

- [ ] **Step 4: Stage and propose commit**

```
git add server/ts-llm/src/model.rs server/ts-llm/Cargo.toml
git status
```

Tell the user: "Task 1 ready — ParsedInput extended, 4 new serialization tests pass. Ready to commit as `feat(ts-llm): extend ParsedInput with messages/system/tools/sampling`. Say the word and I'll commit."

Wait for user confirmation before running `git commit`.

---

## Task 2: Plumb `parsed_input` through `enrich_single`

**Files:**
- Modify: `server/ts-api/src/routes/turn_call_enrichment.rs`

- [ ] **Step 1: Write a failing test for the new field**

Add this test inside the existing `mod tests` block in `server/ts-api/src/routes/turn_call_enrichment.rs`:

```rust
#[test]
fn enrich_single_populates_parsed_input_from_request_body() {
    let reg = build_default_wire_api_registry();
    let req_body = r#"{"model":"claude-3","system":"be helpful","messages":[{"role":"user","content":"hi"}]}"#;
    let detail = CallDetail {
        request_body: Some(req_body.into()),
        ..mk_call_detail(ANTHROPIC, Some(&anthropic_tool_use_body()), None)
    };
    let enriched = enrich_single(detail, &reg);
    assert_eq!(enriched.parsed_input.system.as_deref(), Some("be helpful"));
}

#[test]
fn enrich_single_parsed_input_empty_when_request_body_missing() {
    let reg = build_default_wire_api_registry();
    let detail = mk_call_detail(ANTHROPIC, Some(&anthropic_tool_use_body()), None);
    // request_body is None by default in mk_call_detail; confirm:
    assert!(detail.request_body.is_none());
    let enriched = enrich_single(detail, &reg);
    assert!(enriched.parsed_input.system.is_none());
    assert!(enriched.parsed_input.messages.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

```
cd server && cargo test -p ts-api enrich_single_populates_parsed_input_from_request_body
```

Expected: FAIL — `parsed_input` field does not exist on `EnrichedCallDetail` yet. Compile error is acceptable as the "failing" state.

- [ ] **Step 3: Add `parsed_input` to `EnrichedCallDetail` and populate it**

Edit `server/ts-api/src/routes/turn_call_enrichment.rs`.

In the imports, add `ParsedInput` alongside the existing `ParsedInput` import (if `ParsedInput` is already imported it stays; nothing to change on that line):

```rust
use ts_llm::model::{ParsedInput, ParsedOutput, WireApi};
```

Change the `EnrichedCallDetail` struct:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct EnrichedCallDetail {
    #[serde(flatten)]
    pub base: CallDetail,
    pub parsed: ParsedCallContent,
    pub parsed_input: ParsedInput,
}
```

Update `enrich_single` to thread the parsed input through. Replace the current body of `enrich_single` (from the `pub fn enrich_single(...)` line through the final `}`) with:

```rust
pub fn enrich_single(detail: CallDetail, registry: &WireApiRegistry) -> EnrichedCallDetail {
    let wire = registry.find_by_name(&detail.wire_api);
    let (parsed_out, parsed_in) = wire
        .map(|w| parse_bodies(w, detail.request_body.as_deref(), detail.response_body.as_deref()))
        .unwrap_or_default();
    let next_in = wire
        .map(|w| {
            w.parse_input(
                &serde_json::from_str(
                    detail.next_call_request_body.as_deref().unwrap_or("null"),
                )
                .unwrap_or(serde_json::Value::Null),
            )
        })
        .unwrap_or_default();

    let tool_calls = parsed_out
        .tool_calls
        .into_iter()
        .map(|tc| {
            let result = next_in
                .tool_results
                .iter()
                .find(|tr| tr.tool_use_id == tc.id)
                .map(|tr| {
                    let is_error = tr.is_error;
                    let kind: &'static str = if is_error { "error" } else { "text" };
                    ToolResultFull {
                        size_bytes: tr.content.len() as u64,
                        kind,
                        is_error,
                        content: tr.content.clone(),
                    }
                });
            EnrichedToolCallFull {
                id: tc.id,
                name: tc.name,
                args_json: tc.args_json,
                result,
            }
        })
        .collect();

    EnrichedCallDetail {
        base: detail,
        parsed: ParsedCallContent {
            reasoning: parsed_out.reasoning,
            message: parsed_out.message,
            tool_calls,
        },
        parsed_input: parsed_in,
    }
}
```

The only structural changes from the current implementation: capture `parsed_in` from `parse_bodies` (instead of discarding it), and include it as `parsed_input` in the returned struct.

- [ ] **Step 4: Run test to verify it passes**

```
cd server && cargo test -p ts-api enrich_single_populates_parsed_input_from_request_body enrich_single_parsed_input_empty_when_request_body_missing
```

Expected: PASS. Also run the full ts-api test suite to catch regressions:

```
cd server && cargo test -p ts-api
```

Expected: all tests pass. The existing `enrich_single_populates_tool_result` and `enrich_single_result_none_when_no_next_call` tests still pass because their expectations target `.parsed.tool_calls`, not `.parsed_input`.

- [ ] **Step 5: Stage and propose commit**

```
git add server/ts-api/src/routes/turn_call_enrichment.rs
git status
```

Report: "Task 2 ready — `/api/llm-calls/{id}` now returns `parsed_input`. All ts-api tests pass." Wait for user confirmation before `git commit`.

---

## Task 3: Extend Anthropic `parse_input` — full messages, system, tools, sampling

**Files:**
- Create: `server/ts-llm/tests/fixtures/anthropic_input_full.json`
- Modify: `server/ts-llm/src/wire_apis/anthropic.rs`

- [ ] **Step 1: Create the rich fixture**

Write `server/ts-llm/tests/fixtures/anthropic_input_full.json`:

```json
{
  "model": "claude-sonnet-4-5",
  "system": "You are a helpful assistant.",
  "max_tokens": 8192,
  "temperature": 0.7,
  "top_p": 0.95,
  "stream": true,
  "stop_sequences": ["STOP"],
  "tool_choice": { "type": "auto" },
  "tools": [
    {
      "name": "read_file",
      "description": "Read the contents of a file.",
      "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } }
    },
    {
      "name": "write_file",
      "description": "Write contents to a file.",
      "input_schema": { "type": "object" }
    }
  ],
  "messages": [
    { "role": "user", "content": "hello" },
    {
      "role": "assistant",
      "content": [
        { "type": "text", "text": "let me check" },
        { "type": "tool_use", "id": "toolu_abc", "name": "read_file", "input": { "path": "foo.txt" } }
      ]
    },
    {
      "role": "user",
      "content": [
        { "type": "tool_result", "tool_use_id": "toolu_abc", "content": "file bytes", "is_error": false }
      ]
    },
    {
      "role": "user",
      "content": [
        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
        { "type": "text", "text": "what is in this picture?" }
      ]
    }
  ]
}
```

- [ ] **Step 2: Write failing tests**

At the bottom of `mod tests` in `server/ts-llm/src/wire_apis/anthropic.rs`, add:

```rust
fn anthropic_full_input() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/anthropic_input_full.json"
    ))
    .unwrap()
}

#[test]
fn parse_input_full_system_extracted() {
    let out = AnthropicWireApi.parse_input(&anthropic_full_input());
    assert_eq!(out.system.as_deref(), Some("You are a helpful assistant."));
}

#[test]
fn parse_input_full_messages_order_and_roles() {
    use crate::model::{ParsedContentBlock, ParsedRole};
    let out = AnthropicWireApi.parse_input(&anthropic_full_input());
    // Message 3 ("role":"user", content is exclusively tool_result) should be re-tagged to Tool.
    let roles: Vec<ParsedRole> = out.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            ParsedRole::User,      // "hello"
            ParsedRole::Assistant, // text + tool_use
            ParsedRole::Tool,      // tool_result-only
            ParsedRole::User,      // mixed image + text
        ]
    );
    // The assistant message has exactly 2 content blocks: Text + ToolUse.
    let assistant = &out.messages[1];
    assert_eq!(assistant.content.len(), 2);
    assert!(matches!(assistant.content[0], ParsedContentBlock::Text { .. }));
    assert!(matches!(
        &assistant.content[1],
        ParsedContentBlock::ToolUse { name, .. } if name == "read_file"
    ));
    // The tool-role message has one ToolResult block.
    let tool_msg = &out.messages[2];
    assert_eq!(tool_msg.content.len(), 1);
    assert!(matches!(
        &tool_msg.content[0],
        ParsedContentBlock::ToolResult { tool_use_id, content, is_error }
            if tool_use_id == "toolu_abc" && content == "file bytes" && !*is_error
    ));
    // The last user message has Image + Text.
    let last = &out.messages[3];
    assert_eq!(last.content.len(), 2);
    assert!(matches!(
        &last.content[0],
        ParsedContentBlock::Image { mime, .. } if mime.as_deref() == Some("image/png")
    ));
    assert!(matches!(&last.content[1], ParsedContentBlock::Text { .. }));
}

#[test]
fn parse_input_full_tools_extracted() {
    let out = AnthropicWireApi.parse_input(&anthropic_full_input());
    assert_eq!(out.tools.len(), 2);
    assert_eq!(out.tools[0].name, "read_file");
    assert_eq!(
        out.tools[0].description.as_deref(),
        Some("Read the contents of a file.")
    );
    // input_schema stored as a JSON string — parse it back to verify shape.
    let schema: serde_json::Value =
        serde_json::from_str(&out.tools[0].input_schema_json).unwrap();
    assert_eq!(schema["type"], "object");
}

#[test]
fn parse_input_full_sampling_extracted() {
    let out = AnthropicWireApi.parse_input(&anthropic_full_input());
    assert_eq!(out.sampling.temperature, Some(0.7));
    assert_eq!(out.sampling.top_p, Some(0.95));
    assert_eq!(out.sampling.max_tokens, Some(8192));
    assert_eq!(out.sampling.stream, Some(true));
    assert_eq!(out.sampling.stop, vec!["STOP".to_string()]);
    // tool_choice was an object; stored as serialized JSON string.
    assert_eq!(
        out.sampling.tool_choice.as_deref(),
        Some(r#"{"type":"auto"}"#)
    );
}

#[test]
fn parse_input_preserves_unknown_content_block() {
    use crate::model::ParsedContentBlock;
    let body = serde_json::json!({
        "model": "claude-3",
        "system": "s",
        "messages": [
            { "role": "user", "content": [ { "type": "future_kind", "foo": 1 } ] }
        ]
    });
    let out = AnthropicWireApi.parse_input(&body);
    assert_eq!(out.messages.len(), 1);
    assert_eq!(out.messages[0].content.len(), 1);
    assert!(matches!(out.messages[0].content[0], ParsedContentBlock::Unknown(_)));
}
```

Also update the existing `parse_input_user_only` and `parse_input_with_tool_result` tests if they break because the new fields default to empty — they shouldn't break because those tests only assert on `user_message` / `tool_results`. Run them after implementation to confirm.

- [ ] **Step 3: Run tests to verify they fail**

```
cd server && cargo test -p ts-llm parse_input_full
```

Expected: FAIL — `parse_input` doesn't populate `messages`, `system`, `tools`, `sampling` yet. The existing `parse_input_user_only` and `parse_input_with_tool_result` still pass.

- [ ] **Step 4: Implement the extended Anthropic parser**

Replace the `fn parse_input(&self, body: &Value) -> crate::model::ParsedInput { ... }` block in `server/ts-llm/src/wire_apis/anthropic.rs` with:

```rust
fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
    use crate::model::{
        ParsedContentBlock, ParsedInput, ParsedMessage, ParsedRole, ParsedSampling,
        ParsedToolDef, ParsedToolResult,
    };
    let mut out = ParsedInput::default();

    // system (top-level string)
    if let Some(s) = body.get("system").and_then(|v| v.as_str()) {
        out.system = Some(s.to_string());
    }

    // tools
    if let Some(arr) = body.get("tools").and_then(|v| v.as_array()) {
        for t in arr {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let description = t
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let input_schema_json = t
                .get("input_schema")
                .map(|v| serde_json::to_string(v).unwrap_or_default())
                .unwrap_or_default();
            out.tools.push(ParsedToolDef {
                name,
                description,
                input_schema_json,
            });
        }
    }

    // sampling
    out.sampling = ParsedSampling {
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: body.get("max_tokens").and_then(|v| v.as_u64()).map(|v| v as u32),
        top_p: body.get("top_p").and_then(|v| v.as_f64()),
        top_k: body.get("top_k").and_then(|v| v.as_u64()).map(|v| v as u32),
        stream: body.get("stream").and_then(|v| v.as_bool()),
        tool_choice: body
            .get("tool_choice")
            .map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(v).unwrap_or_default(),
            }),
        stop: body
            .get("stop_sequences")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        response_format: None, // Anthropic doesn't have this.
    };

    // messages
    let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
        return out;
    };
    for msg in messages {
        let wire_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let mut role = match wire_role {
            "user" => ParsedRole::User,
            "assistant" => ParsedRole::Assistant,
            _ => continue, // Anthropic only allows user / assistant at the message level.
        };

        let mut blocks: Vec<ParsedContentBlock> = Vec::new();
        let content = msg.get("content");

        // Content can be a string OR an array of blocks.
        if let Some(s) = content.and_then(|v| v.as_str()) {
            blocks.push(ParsedContentBlock::Text { text: s.to_string() });
            // Maintain the legacy user_message field for the turn joiner.
            if wire_role == "user" {
                out.user_message = Some(s.to_string());
            }
        } else if let Some(arr) = content.and_then(|v| v.as_array()) {
            let mut user_text_buf = String::new();
            for block in arr {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if wire_role == "user" {
                            if !user_text_buf.is_empty() {
                                user_text_buf.push('\n');
                            }
                            user_text_buf.push_str(&text);
                        }
                        blocks.push(ParsedContentBlock::Text { text });
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args_json = block
                            .get("input")
                            .map(|v| serde_json::to_string(v).unwrap_or_default())
                            .unwrap_or_default();
                        blocks.push(ParsedContentBlock::ToolUse { id, name, args_json });
                    }
                    Some("tool_result") => {
                        let tool_use_id = block
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let is_error = block
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let content_str = match block.get("content") {
                            Some(c) if c.is_string() => c.as_str().unwrap().to_string(),
                            Some(c) if c.is_array() => c
                                .as_array()
                                .unwrap()
                                .iter()
                                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            Some(c) => serde_json::to_string(c).unwrap_or_default(),
                            None => String::new(),
                        };
                        // Legacy turn-joiner field.
                        out.tool_results.push(ParsedToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: content_str.clone(),
                            is_error,
                        });
                        blocks.push(ParsedContentBlock::ToolResult {
                            tool_use_id,
                            content: content_str,
                            is_error,
                        });
                    }
                    Some("image") => {
                        let mime = block
                            .get("source")
                            .and_then(|s| s.get("media_type"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        blocks.push(ParsedContentBlock::Image {
                            mime,
                            size_bytes: None,
                        });
                    }
                    _ => {
                        blocks.push(ParsedContentBlock::Unknown(block.clone()));
                    }
                }
            }
            if wire_role == "user" && !user_text_buf.is_empty() {
                out.user_message = Some(user_text_buf);
            }
        }

        // Re-tag user messages whose content is exclusively tool_result blocks.
        if role == ParsedRole::User
            && !blocks.is_empty()
            && blocks
                .iter()
                .all(|b| matches!(b, ParsedContentBlock::ToolResult { .. }))
        {
            role = ParsedRole::Tool;
        }

        out.messages.push(ParsedMessage { role, content: blocks });
    }

    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

```
cd server && cargo test -p ts-llm --lib anthropic
```

Expected: all tests pass — both the 5 new `parse_input_full_*` / `parse_input_preserves_unknown_content_block` tests and the existing `parse_input_user_only` / `parse_input_with_tool_result`.

Run the full ts-llm suite to confirm no regressions:

```
cd server && cargo test -p ts-llm
```

Expected: all pass.

- [ ] **Step 6: Stage and propose commit**

```
git add server/ts-llm/src/wire_apis/anthropic.rs server/ts-llm/tests/fixtures/anthropic_input_full.json
git status
```

Report: "Task 3 ready — Anthropic parse_input populates messages/system/tools/sampling with tool_result role re-tag + unknown block preservation. All ts-llm tests pass." Wait for confirmation before commit.

---

## Task 4: Extend OpenAI Chat `parse_input`

**Files:**
- Create: `server/ts-llm/tests/fixtures/openai_chat_input_full.json`
- Modify: `server/ts-llm/src/wire_apis/openai.rs`

- [ ] **Step 1: Create the fixture**

Write `server/ts-llm/tests/fixtures/openai_chat_input_full.json`:

```json
{
  "model": "gpt-4o-2024-11-20",
  "temperature": 0.3,
  "max_tokens": 2048,
  "top_p": 1.0,
  "stream": true,
  "stop": ["\n\n"],
  "tool_choice": "auto",
  "response_format": { "type": "json_object" },
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Get weather for a city.",
        "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
      }
    }
  ],
  "messages": [
    { "role": "system", "content": "You are a weather bot." },
    { "role": "user", "content": "What's the weather in SF?" },
    {
      "role": "assistant",
      "content": null,
      "tool_calls": [
        {
          "id": "call_1",
          "type": "function",
          "function": { "name": "get_weather", "arguments": "{\"city\":\"SF\"}" }
        }
      ]
    },
    { "role": "tool", "tool_call_id": "call_1", "content": "72F sunny" },
    { "role": "assistant", "content": "It's 72F and sunny in SF." }
  ]
}
```

- [ ] **Step 2: Write failing tests**

Append to `mod tests` in `server/ts-llm/src/wire_apis/openai.rs`:

```rust
fn openai_chat_full_input() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/openai_chat_input_full.json"
    ))
    .unwrap()
}

#[test]
fn chat_parse_input_full_messages_roles() {
    use crate::model::{ParsedContentBlock, ParsedRole};
    let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
    let roles: Vec<ParsedRole> = out.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            ParsedRole::System,
            ParsedRole::User,
            ParsedRole::Assistant,
            ParsedRole::Tool,
            ParsedRole::Assistant,
        ]
    );
    // Assistant-with-tool_calls yields one ToolUse block (no text, since content was null).
    let assistant = &out.messages[2];
    assert_eq!(assistant.content.len(), 1);
    assert!(matches!(
        &assistant.content[0],
        ParsedContentBlock::ToolUse { name, args_json, .. }
            if name == "get_weather" && args_json == "{\"city\":\"SF\"}"
    ));
    // Tool role carries a ToolResult block.
    let tool = &out.messages[3];
    assert_eq!(tool.content.len(), 1);
    assert!(matches!(
        &tool.content[0],
        ParsedContentBlock::ToolResult { tool_use_id, content, .. }
            if tool_use_id == "call_1" && content == "72F sunny"
    ));
}

#[test]
fn chat_parse_input_full_system_stays_in_messages() {
    // OpenAI Chat: top-level `system` field does not exist. System prompt is
    // the first `role=system` message inside `messages[]`.
    let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
    assert!(out.system.is_none());
    assert!(matches!(
        out.messages.first().map(|m| m.role),
        Some(crate::model::ParsedRole::System)
    ));
}

#[test]
fn chat_parse_input_full_tools_extracted() {
    let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
    assert_eq!(out.tools.len(), 1);
    assert_eq!(out.tools[0].name, "get_weather");
    assert_eq!(
        out.tools[0].description.as_deref(),
        Some("Get weather for a city.")
    );
    let schema: serde_json::Value =
        serde_json::from_str(&out.tools[0].input_schema_json).unwrap();
    assert_eq!(schema["type"], "object");
}

#[test]
fn chat_parse_input_full_sampling_extracted() {
    let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
    assert_eq!(out.sampling.temperature, Some(0.3));
    assert_eq!(out.sampling.max_tokens, Some(2048));
    assert_eq!(out.sampling.top_p, Some(1.0));
    assert_eq!(out.sampling.stream, Some(true));
    assert_eq!(out.sampling.stop, vec!["\n\n".to_string()]);
    assert_eq!(out.sampling.tool_choice.as_deref(), Some("auto"));
    assert_eq!(
        out.sampling.response_format.as_deref(),
        Some(r#"{"type":"json_object"}"#)
    );
}
```

- [ ] **Step 3: Run tests to verify they fail**

```
cd server && cargo test -p ts-llm chat_parse_input_full
```

Expected: FAIL. The existing `chat_parse_input_tool_result` still passes (targets `user_message` / `tool_results`).

- [ ] **Step 4: Implement the extended Chat parser**

Replace `OpenAiChatWireApi::parse_input` in `server/ts-llm/src/wire_apis/openai.rs`:

```rust
fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
    use crate::model::{
        ParsedContentBlock, ParsedInput, ParsedMessage, ParsedRole, ParsedSampling,
        ParsedToolDef, ParsedToolResult,
    };
    let mut out = ParsedInput::default();

    // tools — OpenAI Chat: [{type:"function", function:{name, description, parameters}}]
    if let Some(arr) = body.get("tools").and_then(|v| v.as_array()) {
        for t in arr {
            let f = t.get("function");
            let name = f
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let description = f
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let input_schema_json = f
                .and_then(|v| v.get("parameters"))
                .map(|v| serde_json::to_string(v).unwrap_or_default())
                .unwrap_or_default();
            out.tools.push(ParsedToolDef {
                name,
                description,
                input_schema_json,
            });
        }
    }

    // sampling
    out.sampling = ParsedSampling {
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        // Chat uses `max_tokens`; newer models accept `max_completion_tokens`.
        max_tokens: body
            .get("max_tokens")
            .or_else(|| body.get("max_completion_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        top_p: body.get("top_p").and_then(|v| v.as_f64()),
        top_k: None,
        stream: body.get("stream").and_then(|v| v.as_bool()),
        tool_choice: body.get("tool_choice").map(|v| match v.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(v).unwrap_or_default(),
        }),
        stop: match body.get("stop") {
            Some(v) if v.is_string() => vec![v.as_str().unwrap().to_string()],
            Some(v) if v.is_array() => v
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect(),
            _ => Vec::new(),
        },
        response_format: body
            .get("response_format")
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
    };

    // messages
    let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
        return out;
    };
    for msg in messages {
        let wire_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let role = match wire_role {
            "system" => ParsedRole::System,
            "user" => ParsedRole::User,
            "assistant" => ParsedRole::Assistant,
            "tool" => ParsedRole::Tool,
            _ => continue,
        };

        let mut blocks: Vec<ParsedContentBlock> = Vec::new();

        // content: string | array of content parts | null
        if let Some(s) = msg.get("content").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                blocks.push(ParsedContentBlock::Text { text: s.to_string() });
            }
            if wire_role == "user" {
                out.user_message = Some(s.to_string());
            }
        } else if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
            let mut user_text_buf = String::new();
            for part in arr {
                match part.get("type").and_then(|v| v.as_str()) {
                    Some("text") | Some("input_text") => {
                        let text = part
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if wire_role == "user" {
                            if !user_text_buf.is_empty() {
                                user_text_buf.push('\n');
                            }
                            user_text_buf.push_str(&text);
                        }
                        blocks.push(ParsedContentBlock::Text { text });
                    }
                    Some("image_url") => {
                        let mime = part
                            .get("image_url")
                            .and_then(|u| u.get("url"))
                            .and_then(|v| v.as_str())
                            .and_then(|url| {
                                // data:image/png;base64,... → "image/png"
                                url.strip_prefix("data:")
                                    .and_then(|rest| rest.split(';').next())
                                    .map(|s| s.to_string())
                            });
                        blocks.push(ParsedContentBlock::Image { mime, size_bytes: None });
                    }
                    _ => {
                        blocks.push(ParsedContentBlock::Unknown(part.clone()));
                    }
                }
            }
            if wire_role == "user" && !user_text_buf.is_empty() {
                out.user_message = Some(user_text_buf);
            }
        }

        // Assistant tool_calls → ToolUse blocks appended after any text content.
        if wire_role == "assistant" {
            if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let f = tc.get("function");
                    let name = f
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_json = f
                        .and_then(|v| v.get("arguments"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    blocks.push(ParsedContentBlock::ToolUse { id, name, args_json });
                }
            }
        }

        // Tool-role message → ToolResult block.
        if wire_role == "tool" {
            let tool_use_id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content_str = msg
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| msg.get("content").map(|v| v.to_string()))
                .unwrap_or_default();
            // Legacy turn-joiner field.
            out.tool_results.push(ParsedToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content_str.clone(),
                is_error: false,
            });
            // Replace any accumulated blocks — tool messages carry exactly one ToolResult.
            blocks = vec![ParsedContentBlock::ToolResult {
                tool_use_id,
                content: content_str,
                is_error: false,
            }];
        }

        out.messages.push(ParsedMessage { role, content: blocks });
    }

    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

```
cd server && cargo test -p ts-llm --lib chat_parse_input_full chat_parse_input_tool_result
```

Expected: all 5 chat tests pass (4 new + existing one).

```
cd server && cargo test -p ts-llm
```

Expected: entire ts-llm suite passes.

- [ ] **Step 6: Stage and propose commit**

```
git add server/ts-llm/src/wire_apis/openai.rs server/ts-llm/tests/fixtures/openai_chat_input_full.json
git status
```

Report: "Task 4 ready — OpenAI Chat parse_input extended." Wait for confirmation.

---

## Task 5: Extend OpenAI Responses `parse_input`

**Files:**
- Create: `server/ts-llm/tests/fixtures/openai_responses_input_full.json`
- Modify: `server/ts-llm/src/wire_apis/openai.rs`

- [ ] **Step 1: Create the fixture**

Write `server/ts-llm/tests/fixtures/openai_responses_input_full.json`:

```json
{
  "model": "gpt-5-2025-03-01",
  "instructions": "You are a code assistant.",
  "temperature": 0.2,
  "max_completion_tokens": 4096,
  "top_p": 1.0,
  "stream": true,
  "tool_choice": "auto",
  "tools": [
    {
      "type": "function",
      "name": "run_shell",
      "description": "Run a shell command.",
      "parameters": { "type": "object" }
    }
  ],
  "input": [
    {
      "type": "message",
      "role": "user",
      "content": [
        { "type": "input_text", "text": "list the files" }
      ]
    },
    {
      "type": "function_call",
      "call_id": "call_abc",
      "name": "run_shell",
      "arguments": "{\"cmd\":\"ls\"}"
    },
    {
      "type": "function_call_output",
      "call_id": "call_abc",
      "output": "a.txt\nb.txt"
    }
  ]
}
```

- [ ] **Step 2: Write failing tests**

Append to `mod tests` in `server/ts-llm/src/wire_apis/openai.rs`:

```rust
fn openai_responses_full_input() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/openai_responses_input_full.json"
    ))
    .unwrap()
}

#[test]
fn responses_parse_input_full_instructions_become_system() {
    let out = OpenAiResponsesWireApi.parse_input(&openai_responses_full_input());
    assert_eq!(out.system.as_deref(), Some("You are a code assistant."));
}

#[test]
fn responses_parse_input_full_input_string_maps_to_user_message() {
    let body = serde_json::json!({
        "model": "gpt-5",
        "input": "hi"
    });
    let out = OpenAiResponsesWireApi.parse_input(&body);
    use crate::model::{ParsedContentBlock, ParsedRole};
    assert_eq!(out.messages.len(), 1);
    assert_eq!(out.messages[0].role, ParsedRole::User);
    assert!(matches!(
        &out.messages[0].content[0],
        ParsedContentBlock::Text { text } if text == "hi"
    ));
}

#[test]
fn responses_parse_input_full_items_roles_and_blocks() {
    use crate::model::{ParsedContentBlock, ParsedRole};
    let out = OpenAiResponsesWireApi.parse_input(&openai_responses_full_input());
    let roles: Vec<ParsedRole> = out.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![ParsedRole::User, ParsedRole::Assistant, ParsedRole::Tool,]
    );
    // assistant message carries one ToolUse block
    assert!(matches!(
        &out.messages[1].content[0],
        ParsedContentBlock::ToolUse { id, name, args_json }
            if id == "call_abc" && name == "run_shell" && args_json == "{\"cmd\":\"ls\"}"
    ));
    // tool message carries one ToolResult block
    assert!(matches!(
        &out.messages[2].content[0],
        ParsedContentBlock::ToolResult { tool_use_id, content, .. }
            if tool_use_id == "call_abc" && content == "a.txt\nb.txt"
    ));
}

#[test]
fn responses_parse_input_full_tools_and_sampling() {
    let out = OpenAiResponsesWireApi.parse_input(&openai_responses_full_input());
    assert_eq!(out.tools.len(), 1);
    assert_eq!(out.tools[0].name, "run_shell");
    assert_eq!(out.sampling.temperature, Some(0.2));
    assert_eq!(out.sampling.max_tokens, Some(4096));
    assert_eq!(out.sampling.top_p, Some(1.0));
    assert_eq!(out.sampling.stream, Some(true));
    assert_eq!(out.sampling.tool_choice.as_deref(), Some("auto"));
}
```

- [ ] **Step 3: Run tests to verify they fail**

```
cd server && cargo test -p ts-llm responses_parse_input_full
```

Expected: FAIL.

- [ ] **Step 4: Implement the extended Responses parser**

Replace `OpenAiResponsesWireApi::parse_input` in `server/ts-llm/src/wire_apis/openai.rs`:

```rust
fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
    use crate::model::{
        ParsedContentBlock, ParsedInput, ParsedMessage, ParsedRole, ParsedSampling,
        ParsedToolDef, ParsedToolResult,
    };
    let mut out = ParsedInput::default();

    // instructions → system
    if let Some(s) = body.get("instructions").and_then(|v| v.as_str()) {
        out.system = Some(s.to_string());
    }

    // tools — Responses flavor: top-level {type:"function", name, description, parameters}.
    if let Some(arr) = body.get("tools").and_then(|v| v.as_array()) {
        for t in arr {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let description = t
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let input_schema_json = t
                .get("parameters")
                .map(|v| serde_json::to_string(v).unwrap_or_default())
                .unwrap_or_default();
            out.tools.push(ParsedToolDef {
                name,
                description,
                input_schema_json,
            });
        }
    }

    // sampling
    out.sampling = ParsedSampling {
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        // Responses uses `max_completion_tokens`; accept `max_tokens` too for robustness.
        max_tokens: body
            .get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        top_p: body.get("top_p").and_then(|v| v.as_f64()),
        top_k: None,
        stream: body.get("stream").and_then(|v| v.as_bool()),
        tool_choice: body.get("tool_choice").map(|v| match v.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(v).unwrap_or_default(),
        }),
        stop: match body.get("stop") {
            Some(v) if v.is_string() => vec![v.as_str().unwrap().to_string()],
            Some(v) if v.is_array() => v
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect(),
            _ => Vec::new(),
        },
        response_format: body
            .get("response_format")
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
    };

    // input: string | array of items
    if let Some(s) = body.get("input").and_then(|v| v.as_str()) {
        out.user_message = Some(s.to_string());
        out.messages.push(ParsedMessage {
            role: ParsedRole::User,
            content: vec![ParsedContentBlock::Text { text: s.to_string() }],
        });
        return out;
    }

    let Some(items) = body.get("input").and_then(|v| v.as_array()) else {
        return out;
    };

    for item in items {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("message") => {
                let wire_role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let role = match wire_role {
                    "system" => ParsedRole::System,
                    "user" => ParsedRole::User,
                    "assistant" => ParsedRole::Assistant,
                    _ => continue,
                };
                let mut blocks: Vec<ParsedContentBlock> = Vec::new();
                let mut user_text_buf = String::new();
                if let Some(s) = item.get("content").and_then(|v| v.as_str()) {
                    if wire_role == "user" {
                        out.user_message = Some(s.to_string());
                    }
                    if !s.is_empty() {
                        blocks.push(ParsedContentBlock::Text { text: s.to_string() });
                    }
                } else if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
                    for part in arr {
                        match part.get("type").and_then(|v| v.as_str()) {
                            Some("input_text") | Some("text") | Some("output_text") => {
                                let text = part
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if wire_role == "user" {
                                    if !user_text_buf.is_empty() {
                                        user_text_buf.push('\n');
                                    }
                                    user_text_buf.push_str(&text);
                                }
                                blocks.push(ParsedContentBlock::Text { text });
                            }
                            Some("input_image") => {
                                let mime = part
                                    .get("image_url")
                                    .and_then(|v| v.as_str())
                                    .and_then(|url| {
                                        url.strip_prefix("data:")
                                            .and_then(|rest| rest.split(';').next())
                                            .map(|s| s.to_string())
                                    });
                                blocks.push(ParsedContentBlock::Image { mime, size_bytes: None });
                            }
                            _ => blocks.push(ParsedContentBlock::Unknown(part.clone())),
                        }
                    }
                }
                if wire_role == "user" && !user_text_buf.is_empty() {
                    out.user_message = Some(user_text_buf);
                }
                out.messages.push(ParsedMessage { role, content: blocks });
            }
            Some("function_call") => {
                let id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args_json = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                out.messages.push(ParsedMessage {
                    role: ParsedRole::Assistant,
                    content: vec![ParsedContentBlock::ToolUse { id, name, args_json }],
                });
            }
            Some("function_call_output") => {
                let tool_use_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content_str = item
                    .get("output")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| item.get("output").map(|v| v.to_string()))
                    .unwrap_or_default();
                out.tool_results.push(ParsedToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content_str.clone(),
                    is_error: false,
                });
                out.messages.push(ParsedMessage {
                    role: ParsedRole::Tool,
                    content: vec![ParsedContentBlock::ToolResult {
                        tool_use_id,
                        content: content_str,
                        is_error: false,
                    }],
                });
            }
            _ => {
                // Unknown item type — preserve under an Unknown block in a synthetic User message.
                out.messages.push(ParsedMessage {
                    role: ParsedRole::User,
                    content: vec![ParsedContentBlock::Unknown(item.clone())],
                });
            }
        }
    }

    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

```
cd server && cargo test -p ts-llm --lib responses_parse_input
```

Expected: all 5 tests pass (4 new + existing `responses_parse_input_with_function_call_output`).

```
cd server && cargo test -p ts-llm && cargo test -p ts-api
```

Expected: full ts-llm and ts-api suites pass.

- [ ] **Step 6: Stage and propose commit**

```
git add server/ts-llm/src/wire_apis/openai.rs server/ts-llm/tests/fixtures/openai_responses_input_full.json
git status
```

Report: "Task 5 ready — OpenAI Responses parse_input extended. Full backend path now returns structured parsed_input via /api/llm-calls/{id}." Wait for confirmation.

---

## Task 6: Add `ParsedInput` TypeScript types

**Files:**
- Modify: `console/src/types/api.ts`

- [ ] **Step 1: Add the new types**

Edit `console/src/types/api.ts`. Find the `// LLM call detail types` section (currently around line 162) and insert the new types just above the existing `ParsedCallContent` interface:

```ts
export type ParsedRole = "system" | "user" | "assistant" | "tool"

export type ParsedContentBlock =
  | { type: "text"; text: string }
  | { type: "tool_use"; id: string; name: string; args_json: string }
  | { type: "tool_result"; tool_use_id: string; content: string; is_error: boolean }
  | { type: "image"; mime: string | null; size_bytes: number | null }
  // Forward-compat: an unknown block preserves its original shape (including its
  // own `type` field). The frontend renders it as `⚠️ unknown block: {type}`.
  | ({ type: string } & Record<string, unknown>)

export interface ParsedMessage {
  role: ParsedRole
  content: ParsedContentBlock[]
}

export interface ParsedToolDef {
  name: string
  description: string | null
  input_schema_json: string
}

export interface ParsedSampling {
  temperature: number | null
  max_tokens: number | null
  top_p: number | null
  top_k: number | null
  stream: boolean | null
  tool_choice: string | null
  stop: string[]
  response_format: string | null
}

export interface ParsedInput {
  messages: ParsedMessage[]
  system: string | null
  tools: ParsedToolDef[]
  sampling: ParsedSampling
}
```

Then extend `LlmCallDetail` by adding `parsed_input` at the end. Find the `LlmCallDetail` interface and add `parsed_input: ParsedInput` right after `parsed: ParsedCallContent`:

```ts
export interface LlmCallDetail {
  // … all existing fields stay exactly as-is …
  parsed: ParsedCallContent
  parsed_input: ParsedInput  // NEW
}
```

- [ ] **Step 2: Verify the console still typechecks**

```
cd console && bun run build
```

Expected: `tsc -b && vite build` succeeds. No new usages of `parsed_input` exist yet, so adding it can't break consumers.

- [ ] **Step 3: Stage and propose commit**

```
git add console/src/types/api.ts
git status
```

Report: "Task 6 ready — LlmCallDetail type includes parsed_input. Typecheck passes." Wait for confirmation.

---

## Task 7: Extract shared `<CallParsedOutput />` component

**Files:**
- Create: `console/src/components/call-parsed-output.tsx`
- Modify: `console/src/components/turn-detail/call-card.tsx`

- [ ] **Step 1: Create the shared output component**

Write `console/src/components/call-parsed-output.tsx`:

```tsx
import { useState } from "react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import type { EnrichedToolCallFull, ParsedCallContent } from "@/types/api"

function formatArgs(s: string): string {
  try { return JSON.stringify(JSON.parse(s), null, 2) } catch { return s }
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

function ToolCallRow({ tc }: { tc: EnrichedToolCallFull }) {
  const [argsOpen, setArgsOpen] = useState(true)
  const [resultOpen, setResultOpen] = useState(false)
  return (
    <div className="rounded bg-muted/40 p-2">
      <div className="font-medium">🔧 {tc.name}</div>
      <details className="mt-1" open={argsOpen} onToggle={(e) => setArgsOpen((e.target as HTMLDetailsElement).open)}>
        <summary className="cursor-pointer text-muted-foreground text-[10px]">args</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatArgs(tc.args_json)}</pre>
      </details>
      {tc.result ? (
        <details className="mt-1" open={resultOpen} onToggle={(e) => setResultOpen((e.target as HTMLDetailsElement).open)}>
          <summary className={cn("cursor-pointer text-[10px]", tc.result.is_error ? "text-red-600" : "text-muted-foreground")}>
            ⤷ {tc.result.is_error ? "error" : "result"} · {formatSize(tc.result.size_bytes)}
          </summary>
          <pre className={cn(
            "mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]",
            tc.result.is_error && "text-red-600",
          )}>
            {tc.result.content}
          </pre>
        </details>
      ) : (
        <div className="mt-1 text-[10px] text-muted-foreground italic">⤷ result · (no response, turn ended)</div>
      )}
    </div>
  )
}

interface Props {
  parsed: ParsedCallContent
}

export function CallParsedOutput({ parsed }: Props) {
  return (
    <div className="space-y-3">
      {parsed.reasoning && (
        <details className="rounded border border-border/50 p-2" open={false}>
          <summary className="cursor-pointer text-muted-foreground">Reasoning</summary>
          <pre className="mt-2 max-h-[600px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">{parsed.reasoning}</pre>
        </details>
      )}
      {parsed.message && (
        <details className="rounded border border-border/50 p-2" open>
          <summary className="cursor-pointer text-muted-foreground">Message</summary>
          <div className="mt-2 max-h-[400px] overflow-auto text-[11px]">
            <Markdown text={parsed.message} />
          </div>
        </details>
      )}
      {parsed.tool_calls && parsed.tool_calls.length > 0 && (
        <div className="rounded border border-border/50 p-2">
          <div className="mb-1 text-muted-foreground">Tool calls ({parsed.tool_calls.length})</div>
          <div className="space-y-2">
            {parsed.tool_calls.map((tc) => (
              <ToolCallRow key={tc.id} tc={tc} />
            ))}
          </div>
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 2: Update `call-card.tsx` to use the shared component**

Edit `console/src/components/turn-detail/call-card.tsx`.

Remove these imports that are no longer needed here (they now live inside `call-parsed-output.tsx`):
- `Markdown` import
- `EnrichedToolCallFull` type import

Add the import:

```ts
import { CallParsedOutput } from "@/components/call-parsed-output"
```

Remove the local `ToolCallRow` function, `formatArgs`, and `formatSize` helpers (now in the shared component).

Replace the expanded-content JSX inside `CallCard` — the block currently rendering Reasoning / Message / Tool calls (roughly lines 136–166 of the current file, starting from `{expanded && (` and ending just before `<button onClick={() => onOpenRawHttp?.(call.id)}`) — so the render path becomes:

```tsx
{expanded && (
  <div className="border-t border-border px-3 py-2 space-y-3 text-xs">
    {detail && <CallParsedOutput parsed={detail.parsed} />}
    <div className="text-muted-foreground">
      {call.wire_api} · TTFB {formatMs(call.ttfb_ms)} · finish: {call.finish_reason ?? "—"}
    </div>
    <button onClick={() => onOpenRawHttp?.(call.id)} className="text-foreground hover:underline">View raw HTTP →</button>
  </div>
)}
```

The `formatMs` import stays (still used for TTFB). The `formatNumber` and `useLlmCallDetail` imports stay. The `cn` and `ChevronRight`/`ChevronDown`/`Wrench`/`MessageSquare`/`Target` imports stay.

- [ ] **Step 3: Verify Turn Detail still renders correctly**

Run the dev server and open an existing Agent Turn:

```
cd console && bun run dev
```

Open `http://localhost:5173/agent-turns` in a browser, click a turn, click any call to expand it.

Expected: Reasoning / Message / Tool calls render exactly as before. No visual regression.

Also run the typecheck:

```
cd console && bun run build
```

Expected: build succeeds.

- [ ] **Step 4: Stage and propose commit**

```
git add console/src/components/call-parsed-output.tsx console/src/components/turn-detail/call-card.tsx
git status
```

Report: "Task 7 ready — CallParsedOutput extracted; CallCard uses shared component; Turn Detail verified visually unchanged." Wait for confirmation.

---

## Task 8: Build `<SamplingBlock />`

**Files:**
- Create: `console/src/components/llm-call-detail/sampling-block.tsx`

- [ ] **Step 1: Implement the block**

Write `console/src/components/llm-call-detail/sampling-block.tsx`:

```tsx
import type { ParsedSampling } from "@/types/api"

interface Props {
  sampling: ParsedSampling
}

function pairs(s: ParsedSampling): string[] {
  const out: string[] = []
  if (s.temperature != null) out.push(`temp=${s.temperature}`)
  if (s.max_tokens != null) out.push(`max_tokens=${s.max_tokens}`)
  if (s.top_p != null) out.push(`top_p=${s.top_p}`)
  if (s.top_k != null) out.push(`top_k=${s.top_k}`)
  if (s.stream != null) out.push(`stream=${s.stream}`)
  if (s.tool_choice) out.push(`tool_choice=${s.tool_choice}`)
  if (s.stop.length > 0) out.push(`stop=${JSON.stringify(s.stop)}`)
  if (s.response_format) out.push(`response_format=${s.response_format}`)
  return out
}

export function SamplingBlock({ sampling }: Props) {
  const items = pairs(sampling)
  return (
    <div className="rounded border border-border/60 bg-background px-3 py-2 text-xs">
      <span className="font-medium">Sampling</span>
      <span className="ml-2 text-muted-foreground">
        {items.length > 0 ? items.join(" · ") : "(defaults)"}
      </span>
    </div>
  )
}
```

- [ ] **Step 2: Typecheck**

```
cd console && bun run build
```

Expected: build succeeds.

- [ ] **Step 3: Stage and propose commit**

```
git add console/src/components/llm-call-detail/sampling-block.tsx
git status
```

Report: "Task 8 ready — SamplingBlock in place."

---

## Task 9: Build `<ToolsBlock />`

**Files:**
- Create: `console/src/components/llm-call-detail/tools-block.tsx`

- [ ] **Step 1: Implement the block**

Write `console/src/components/llm-call-detail/tools-block.tsx`:

```tsx
import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import type { ParsedToolDef } from "@/types/api"

interface Props {
  tools: ParsedToolDef[]
}

function formatJson(raw: string): string {
  try { return JSON.stringify(JSON.parse(raw), null, 2) } catch { return raw }
}

function ToolRow({ tool }: { tool: ParsedToolDef }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">{tool.name}</span>
        {tool.description && (
          <span className="truncate text-muted-foreground" title={tool.description}>
            — {tool.description}
          </span>
        )}
      </button>
      {open && (
        <pre className="mx-3 mb-2 max-h-[300px] overflow-auto rounded bg-muted p-2 font-mono text-[10px]">
          {formatJson(tool.input_schema_json || "{}")}
        </pre>
      )}
    </div>
  )
}

export function ToolsBlock({ tools }: Props) {
  const [open, setOpen] = useState(false)
  if (tools.length === 0) return null
  const teaser = tools.slice(0, 3).map((t) => t.name).join(", ")
  const more = tools.length > 3 ? `, +${tools.length - 3}` : ""
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Tools</span>
        <span className="text-muted-foreground">({tools.length})</span>
        {!open && (
          <span className="truncate text-muted-foreground">— {teaser}{more}</span>
        )}
      </button>
      {open && (
        <div className="border-t border-border/40">
          {tools.map((t) => <ToolRow key={t.name} tool={t} />)}
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 2: Typecheck**

```
cd console && bun run build
```

Expected: build succeeds.

- [ ] **Step 3: Stage and propose commit**

```
git add console/src/components/llm-call-detail/tools-block.tsx
git status
```

Report: "Task 9 ready."

---

## Task 10: Build `<SystemBlock />`

**Files:**
- Create: `console/src/components/llm-call-detail/system-block.tsx`

- [ ] **Step 1: Implement**

Write `console/src/components/llm-call-detail/system-block.tsx`:

```tsx
import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { Markdown } from "@/components/ui/markdown"

interface Props {
  system: string | null
}

export function SystemBlock({ system }: Props) {
  const [open, setOpen] = useState(false)
  if (!system) return null
  const chars = system.length
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">System Prompt</span>
        <span className="text-muted-foreground">({chars.toLocaleString()} chars)</span>
      </button>
      {open && (
        <div className="max-h-[400px] overflow-auto border-t border-border/40 p-3">
          <Markdown text={system} />
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 2: Typecheck**

```
cd console && bun run build
```

Expected: build succeeds.

- [ ] **Step 3: Stage and propose commit**

```
git add console/src/components/llm-call-detail/system-block.tsx
git status
```

Report: "Task 10 ready."

---

## Task 11: Build `<MessagesBlock />`

**Files:**
- Create: `console/src/components/llm-call-detail/messages-block.tsx`

- [ ] **Step 1: Implement**

Write `console/src/components/llm-call-detail/messages-block.tsx`:

```tsx
import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import type { ParsedContentBlock, ParsedMessage, ParsedRole } from "@/types/api"

const PREVIEW_CHARS = 120

const ROLE_STYLES: Record<ParsedRole, string> = {
  system: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  user: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  assistant: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
  tool: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
}

function formatJson(raw: string): string {
  try { return JSON.stringify(JSON.parse(raw), null, 2) } catch { return raw }
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

function previewOfContent(content: ParsedContentBlock[]): string {
  for (const b of content) {
    if (b.type === "text") return b.text.slice(0, PREVIEW_CHARS)
    if (b.type === "tool_use") return `🔧 ${b.name}(${b.args_json.slice(0, 60)}${b.args_json.length > 60 ? "…" : ""})`
    if (b.type === "tool_result") return `⤷ ${b.tool_use_id} · ${formatSize(b.content.length)}`
    if (b.type === "image") return `🖼️ image${b.mime ? ` (${b.mime})` : ""}`
  }
  return ""
}

function ContentBlockView({ block }: { block: ParsedContentBlock }) {
  if (block.type === "text") {
    return (
      <div className="text-[11px]"><Markdown text={block.text} /></div>
    )
  }
  if (block.type === "tool_use") {
    return (
      <div className="rounded bg-muted/40 p-2 text-[11px]">
        <div className="font-medium">🔧 {block.name}</div>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(block.args_json)}</pre>
      </div>
    )
  }
  if (block.type === "tool_result") {
    return (
      <div className="rounded bg-muted/40 p-2 text-[11px]">
        <div className={cn("mb-1", block.is_error && "text-red-600")}>
          ⤷ {block.is_error ? "error" : "result"} · {formatSize(block.content.length)} · <span className="text-muted-foreground">{block.tool_use_id}</span>
        </div>
        <pre className={cn(
          "max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]",
          block.is_error && "text-red-600",
        )}>{block.content}</pre>
      </div>
    )
  }
  if (block.type === "image") {
    return (
      <div className="text-[11px] text-muted-foreground italic">
        🖼️ image{block.mime ? ` (${block.mime})` : ""}{block.size_bytes != null ? ` · ${formatSize(block.size_bytes)}` : ""}
      </div>
    )
  }
  // Unknown / forward-compat block — preserve as raw JSON.
  return (
    <details className="text-[11px]">
      <summary className="cursor-pointer text-red-600">⚠️ unknown block: {String(block.type)}</summary>
      <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{JSON.stringify(block, null, 2)}</pre>
    </details>
  )
}

function MessageRow({ msg, index }: { msg: ParsedMessage; index: number }) {
  const [open, setOpen] = useState(false)
  const preview = previewOfContent(msg.content)
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        <span className="w-5 shrink-0 text-[10px] tabular-nums text-muted-foreground">#{index + 1}</span>
        <span className={cn("shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium", ROLE_STYLES[msg.role])}>
          {msg.role}
        </span>
        <span className="flex-1 truncate text-muted-foreground">{preview}</span>
        {open ? <ChevronDown className="size-3 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3 shrink-0 text-muted-foreground" />}
      </button>
      {open && (
        <div className="space-y-2 border-t border-border/30 bg-muted/10 px-3 py-2">
          {msg.content.map((b, i) => <ContentBlockView key={i} block={b} />)}
        </div>
      )}
    </div>
  )
}

interface Props {
  messages: ParsedMessage[]
}

export function MessagesBlock({ messages }: Props) {
  const [open, setOpen] = useState(true)
  if (messages.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Messages</span>
        <span className="text-muted-foreground">({messages.length})</span>
      </button>
      {open && (
        <div>
          {messages.map((m, i) => <MessageRow key={i} msg={m} index={i} />)}
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 2: Typecheck**

```
cd console && bun run build
```

Expected: build succeeds.

- [ ] **Step 3: Stage and propose commit**

```
git add console/src/components/llm-call-detail/messages-block.tsx
git status
```

Report: "Task 11 ready."

---

## Task 12: Build `<InputSection />` container

**Files:**
- Create: `console/src/components/llm-call-detail/input-section.tsx`

- [ ] **Step 1: Implement**

Write `console/src/components/llm-call-detail/input-section.tsx`:

```tsx
import type { ParsedInput } from "@/types/api"
import { MessagesBlock } from "./messages-block"
import { SystemBlock } from "./system-block"
import { ToolsBlock } from "./tools-block"
import { SamplingBlock } from "./sampling-block"

interface Props {
  parsedInput: ParsedInput
  wireApi: string
  hasRequestBody: boolean
  onOpenRawHttp: () => void
}

function isEmpty(p: ParsedInput): boolean {
  return (
    p.messages.length === 0 &&
    !p.system &&
    p.tools.length === 0 &&
    p.sampling.temperature == null &&
    p.sampling.max_tokens == null &&
    p.sampling.top_p == null &&
    p.sampling.top_k == null &&
    p.sampling.stream == null &&
    !p.sampling.tool_choice &&
    p.sampling.stop.length === 0 &&
    !p.sampling.response_format
  )
}

export function InputSection({ parsedInput, wireApi, hasRequestBody, onOpenRawHttp }: Props) {
  const empty = isEmpty(parsedInput)
  return (
    <section className="border-l-2 border-muted-foreground/30 pl-3">
      <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
        Input
      </div>
      {!hasRequestBody ? (
        <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
          Request body not captured.
        </div>
      ) : empty ? (
        <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs">
          Could not parse request body as <span className="font-mono">{wireApi}</span>.
          <button onClick={onOpenRawHttp} className="ml-2 text-foreground hover:underline">View raw HTTP →</button>
        </div>
      ) : (
        <div className="space-y-2">
          <MessagesBlock messages={parsedInput.messages} />
          <SystemBlock system={parsedInput.system} />
          <ToolsBlock tools={parsedInput.tools} />
          <SamplingBlock sampling={parsedInput.sampling} />
        </div>
      )}
    </section>
  )
}
```

- [ ] **Step 2: Typecheck**

```
cd console && bun run build
```

Expected: build succeeds.

- [ ] **Step 3: Stage and propose commit**

```
git add console/src/components/llm-call-detail/input-section.tsx
git status
```

Report: "Task 12 ready."

---

## Task 13: Lift Summary / Timeline / Metadata into shared components

**Files:**
- Create: `console/src/components/llm-call-detail/summary-cards.tsx`
- Create: `console/src/components/llm-call-detail/timeline-bar.tsx`
- Create: `console/src/components/llm-call-detail/metadata-grid.tsx`

- [ ] **Step 1: `summary-cards.tsx`**

Write `console/src/components/llm-call-detail/summary-cards.tsx`. The content matches the current `SummaryCard` + four-card grid from `pages/llm-call-detail-panel.tsx`:

```tsx
import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { formatMs, formatNumber } from "@/lib/format"
import type { LlmCallDetail } from "@/types/api"

function SummaryCard({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1 rounded-lg border border-border bg-muted/30 px-3 py-2">
      <span className="text-xs text-muted-foreground">{label}</span>
      <div className="text-sm font-medium">{children}</div>
    </div>
  )
}

interface Props {
  detail: LlmCallDetail
}

export function SummaryCards({ detail }: Props) {
  return (
    <div className="grid grid-cols-4 gap-3">
      <SummaryCard label="Wire API / Model">
        <div>{detail.wire_api}</div>
        <div className="truncate text-xs text-muted-foreground" title={detail.model}>
          {detail.model}
        </div>
      </SummaryCard>
      <SummaryCard label="Status / Finish">
        <div className="flex items-center gap-2">
          <StatusBadge status={detail.status_code} />
          <FinishBadge reason={detail.finish_reason} />
        </div>
      </SummaryCard>
      <SummaryCard label="TTFB / E2E">
        <div className="tabular-nums">{formatMs(detail.ttfb_ms)}</div>
        <div className="text-xs tabular-nums text-muted-foreground">
          {formatMs(detail.e2e_latency_ms)}
        </div>
      </SummaryCard>
      <SummaryCard label="Tokens">
        <div className="flex items-center gap-3 tabular-nums">
          <span className="flex flex-col">
            <span className="text-[10px] text-muted-foreground">in</span>
            <span>{formatNumber(detail.input_tokens)}</span>
          </span>
          <span className="flex flex-col">
            <span className="text-[10px] text-muted-foreground">out</span>
            <span>{formatNumber(detail.output_tokens)}</span>
          </span>
        </div>
        <div className="text-xs tabular-nums text-muted-foreground">
          total: {formatNumber(detail.total_tokens)}
        </div>
      </SummaryCard>
    </div>
  )
}
```

- [ ] **Step 2: `timeline-bar.tsx`**

Write `console/src/components/llm-call-detail/timeline-bar.tsx`:

```tsx
import { formatDateTime, formatMs } from "@/lib/format"
import type { LlmCallDetail } from "@/types/api"

interface Props {
  detail: LlmCallDetail
}

export function TimelineBar({ detail }: Props) {
  const { request_time, complete_time, ttfb_ms, e2e_latency_ms } = detail

  if (!complete_time || !e2e_latency_ms) {
    return (
      <div className="rounded-lg border border-border bg-muted/30 px-4 py-3 text-sm text-muted-foreground">
        Timeline data unavailable
      </div>
    )
  }

  const ttfbRatio = (ttfb_ms ?? 0) / e2e_latency_ms
  const genRatio = 1 - ttfbRatio

  return (
    <div className="rounded-lg border border-border bg-muted/30 px-4 py-3">
      <div className="mb-2 flex justify-between text-xs text-muted-foreground">
        <span>{formatDateTime(request_time)}</span>
        <span>{formatDateTime(complete_time)}</span>
      </div>
      <div className="flex h-6 overflow-hidden rounded-md">
        {ttfbRatio > 0 && (
          <div
            className="flex items-center justify-center bg-amber-400/80 text-xs font-medium text-amber-900 dark:bg-amber-500/30 dark:text-amber-300"
            style={{ width: `${Math.max(ttfbRatio * 100, 8)}%` }}
          >
            TTFB {formatMs(ttfb_ms)}
          </div>
        )}
        {genRatio > 0 && (
          <div
            className="flex items-center justify-center bg-blue-400/80 text-xs font-medium text-blue-900 dark:bg-blue-500/30 dark:text-blue-300"
            style={{ width: `${Math.max(genRatio * 100, 8)}%` }}
          >
            Gen {formatMs(e2e_latency_ms - (ttfb_ms ?? 0))}
          </div>
        )}
      </div>
      <div className="mt-1.5 flex gap-4 text-xs text-muted-foreground">
        <span>TTFB: {formatMs(ttfb_ms)}</span>
        <span>E2E: {formatMs(e2e_latency_ms)}</span>
      </div>
    </div>
  )
}
```

- [ ] **Step 3: `metadata-grid.tsx`**

Write `console/src/components/llm-call-detail/metadata-grid.tsx`:

```tsx
import type { LlmCallDetail } from "@/types/api"

interface Props {
  detail: LlmCallDetail
}

export function MetadataGrid({ detail }: Props) {
  const rows: [string, string][] = [
    ["ID", detail.id],
    ["Response ID", detail.response_id ?? "—"],
    ["Path", detail.request_path],
    ["Client", `${detail.client_ip}:${detail.client_port}`],
    ["Server", `${detail.server_ip}:${detail.server_port}`],
    ["Stream", detail.is_stream ? "Yes" : "No"],
    ["API Type", detail.api_type],
    ["Tenant", detail.tenant_id ?? "—"],
  ]

  return (
    <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 px-4 py-3 text-sm">
      {rows.map(([label, value]) => (
        <div key={label} className="contents">
          <span className="text-muted-foreground">{label}</span>
          <span className="truncate font-mono text-xs" title={value}>
            {value}
          </span>
        </div>
      ))}
    </div>
  )
}
```

- [ ] **Step 4: Typecheck**

```
cd console && bun run build
```

Expected: build succeeds.

- [ ] **Step 5: Stage and propose commit**

```
git add console/src/components/llm-call-detail/summary-cards.tsx console/src/components/llm-call-detail/timeline-bar.tsx console/src/components/llm-call-detail/metadata-grid.tsx
git status
```

Report: "Task 13 ready — summary/timeline/metadata lifted into focused components."

---

## Task 14: Rewrite `LlmCallDetailPanel`

**Files:**
- Modify: `console/src/pages/llm-call-detail-panel.tsx`
- Modify: `console/src/components/turn-detail/raw-http-drawer.tsx` (if its props require a signature change; inspect first)

- [ ] **Step 1: Inspect `RawHttpDrawer` props**

Open `console/src/components/turn-detail/raw-http-drawer.tsx` and confirm the component accepts `callId: string | null` and `onClose: () => void`. If it does, no changes needed here — we can reuse it directly.

Record the result. If the signature is different, adjust Step 2's usage accordingly.

- [ ] **Step 2: Rewrite the panel**

Replace the entire contents of `console/src/pages/llm-call-detail-panel.tsx` with:

```tsx
import { useState } from "react"
import { X, ChevronUp, ChevronDown, Loader2 } from "lucide-react"
import { useLlmCallDetail } from "@/hooks/use-llm-call-detail"
import { RawHttpDrawer } from "@/components/turn-detail/raw-http-drawer"
import { CallParsedOutput } from "@/components/call-parsed-output"
import { SummaryCards } from "@/components/llm-call-detail/summary-cards"
import { TimelineBar } from "@/components/llm-call-detail/timeline-bar"
import { MetadataGrid } from "@/components/llm-call-detail/metadata-grid"
import { InputSection } from "@/components/llm-call-detail/input-section"

interface Props {
  id: string
  onClose: () => void
  onNavigate: (direction: "prev" | "next") => void
  hasPrev: boolean
  hasNext: boolean
}

export function LlmCallDetailPanel({ id, onClose, onNavigate, hasPrev, hasNext }: Props) {
  const { data: detail, isLoading, isError } = useLlmCallDetail(id)
  const [rawOpen, setRawOpen] = useState(false)

  return (
    <>
      {/* Backdrop */}
      <div className="fixed inset-0 z-40 bg-black/20" onClick={onClose} />

      {/* Panel */}
      <div className="fixed top-0 right-0 z-50 flex h-full w-[70%] min-w-[560px] flex-col border-l border-border bg-background shadow-xl animate-in slide-in-from-right duration-200">
        {/* Header */}
        <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">
          <h2 className="text-sm font-semibold">LLM Call Detail</h2>
          <div className="flex items-center gap-1">
            <button
              onClick={() => onNavigate("prev")}
              disabled={!hasPrev}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronUp className="size-4" />
            </button>
            <button
              onClick={() => onNavigate("next")}
              disabled={!hasNext}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronDown className="size-4" />
            </button>
            <button
              onClick={onClose}
              className="ml-2 rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <X className="size-4" />
            </button>
          </div>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto">
          {isLoading ? (
            <div className="flex h-40 items-center justify-center">
              <Loader2 className="size-5 animate-spin text-muted-foreground" />
            </div>
          ) : isError || !detail ? (
            <div className="flex h-40 items-center justify-center text-destructive">
              Failed to load LLM call detail
            </div>
          ) : (
            <div className="flex flex-col gap-4 p-4">
              {/* ① Summary */}
              <SummaryCards detail={detail} />

              {/* ② Timeline */}
              <TimelineBar detail={detail} />

              {/* ③ Metadata */}
              <MetadataGrid detail={detail} />

              {/* ④ Input */}
              <InputSection
                parsedInput={detail.parsed_input}
                wireApi={detail.wire_api}
                hasRequestBody={detail.request_body != null}
                onOpenRawHttp={() => setRawOpen(true)}
              />

              {/* ⑤ Output */}
              <section className="border-l-2 border-emerald-500/40 pl-3">
                <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
                  Output
                </div>
                <CallParsedOutput parsed={detail.parsed} />
              </section>

              {/* ⑥ Raw HTTP link */}
              <div className="flex justify-end border-t border-border pt-3">
                <button
                  onClick={() => setRawOpen(true)}
                  className="text-xs text-muted-foreground hover:text-foreground hover:underline"
                >
                  View raw HTTP →
                </button>
              </div>
            </div>
          )}
        </div>
      </div>

      {/* Raw HTTP drawer */}
      <RawHttpDrawer callId={rawOpen ? id : null} onClose={() => setRawOpen(false)} />
    </>
  )
}
```

- [ ] **Step 3: Typecheck and run the console**

```
cd console && bun run build
```

Expected: build succeeds.

```
cd console && bun run dev
```

Open `http://localhost:5173/llm-calls` and click any row. Expected: the new panel renders with all six sections — summary, timeline, metadata, input (with messages / sampling etc.), output (reasoning / message / tool calls), and a "View raw HTTP →" button at the bottom. Clicking the button opens the `RawHttpDrawer` over the panel.

- [ ] **Step 4: Stage and propose commit**

```
git add console/src/pages/llm-call-detail-panel.tsx
git status
```

Report: "Task 14 ready — LlmCallDetailPanel rewritten; build + dev run pass. Ready for manual verification on a real capture in Task 15." Wait for confirmation.

---

## Task 15: Manual verification on real captures

**Files:** none modified.

- [ ] **Step 1: Start backend + frontend**

```
just dev server    # terminal 1
just dev console   # terminal 2
```

- [ ] **Step 2: Anthropic tool-use call**

Open `/llm-calls`, filter to an Anthropic call with `finish_reason = tool_use`. Confirm:

- Summary cards populated (wire, model, status/finish, TTFB/E2E, tokens).
- Timeline bar shows TTFB + Gen segments.
- Metadata grid present.
- **Input Messages** lists the conversation; the assistant message shows tool_use block; a following user message whose content is only tool_result appears as role=tool with the orange chip.
- **System Prompt** block is present (Anthropic top-level `system`).
- **Tools** collapsed with name teaser; expands to show description + schema.
- **Sampling** shows a single-line compact kv list.
- **Output** section shows message and tool_calls with paired tool_result (when a subsequent call in the same turn carries the result).
- Click "View raw HTTP →" — drawer opens with the 4 original sections (request headers / response headers / request body / response body). Closing the drawer returns to the panel.

- [ ] **Step 3: OpenAI Chat call**

Open an OpenAI Chat call. Confirm:

- **Input Messages** starts with a role=system entry (NOT a separate System Prompt block above the messages).
- **System Prompt** block is absent (because `detail.parsed_input.system === null`).
- Tools / Sampling render.
- Output renders.

- [ ] **Step 4: OpenAI Responses call**

Open an OpenAI Responses call (if the capture has one). Confirm:

- **System Prompt** block shows the `instructions` content.
- Messages include the `input[]` items (user message + function_call + function_call_output).
- Output renders.

- [ ] **Step 5: Edge cases**

- Navigate to a call with a missing `request_body` (look in the list; usually short-lived captures). Confirm the Input section shows "Request body not captured." and the rest still renders.
- Navigate via prev/next chevrons. Confirm they still advance between calls.
- Open a Turn Detail (any turn) and confirm the CallCard expanded state still renders identically to before the refactor — same Reasoning / Message / Tool calls.

- [ ] **Step 6: Run final quality checks**

```
just quality all
```

Expected: format + clippy + tsc + eslint all pass.

- [ ] **Step 7: Report**

Report to the user: "Task 15 complete — manual verification across Anthropic, OpenAI Chat, OpenAI Responses, missing-body edge case, and prev/next navigation. `just quality all` passes. The implementation covers the full spec. Ready to squash/retain commits as you prefer."
