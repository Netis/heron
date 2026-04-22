# LLM Call Detail — Structured IO Redesign

## Context

The current `LlmCallDetailPanel` (`console/src/pages/llm-call-detail-panel.tsx`) renders request/response headers and bodies as four raw collapsible sections. This is almost byte-for-byte identical to `HttpExchangeDetailPanel`, which defeats the purpose of having a separate "LLM Call" entry point: the reader sees HTTP plumbing rather than LLM-level semantics.

The Agent Turn Detail has already evolved in the opposite direction — its `CallCard` expanded state surfaces parsed `reasoning` / `message` / `tool_calls` (with paired `tool_results`) via backend-supplied `ParsedOutput`. That design assumes a turn-centric narrative; standalone `/llm-calls` needs an equivalent but single-call framing.

This redesign recasts `/llm-calls` detail as a **single-call vertical IO inspection** view:

- **Input side** (new): parsed `messages[]`, `system`, `tools[]`, `sampling` — the conversation context this call consumed.
- **Output side** (reused): parsed `reasoning` / `message` / `tool_calls` — what the model produced.
- **Raw HTTP**: kept, but moved behind a single "View raw HTTP →" button that opens the existing `RawHttpDrawer`. The Request/Response Headers + Request/Response Body sections disappear from the primary panel.

Positioning across entries becomes:

| Entry | Purpose |
|---|---|
| `/http-exchanges` | Raw HTTP plumbing — headers, bodies, SSE frames as-is. |
| `/llm-calls` | Single-call LLM semantics — structured input + output for one request/response pair. |
| `/agent-turns` | Multi-call agent narrative — ordered call sequence with user input / final answer bookends. |

Each level reads the same underlying `LlmCall` rows but presents at a different abstraction.

## Target User

**Primary:** Dev / ops debugging a single call — "what did this LLM request actually send, and what did it produce?"

Secondary: SRE inspecting HTTP-level artifacts (raw headers / bytes). They stay supported through the Raw HTTP drawer, unchanged.

## Scope

- Redesign `LlmCallDetailPanel` layout and rendering.
- Extend `ts_llm::model::ParsedInput` with `messages: Vec<ParsedMessage>`, `system: Option<String>`, `tools: Vec<ParsedToolDef>`, `sampling: ParsedSampling`. Keep existing `user_message` and `tool_results` fields unchanged (Turn joiner depends on them).
- Extend `WireApi::parse_input` implementations (`anthropic.rs`, `openai.rs` — OpenAI Chat + OpenAI Responses) to populate the new fields.
- Add `parsed_input` to the `/api/llm-calls/{id}` payload (alongside existing `parsed` output).
- Extract the existing Output rendering (Reasoning / Message / Tool calls) out of `components/turn-detail/call-card.tsx` into a shared `components/call-parsed-output.tsx`, reused by both Turn Detail and the new LLM Call Detail.
- Update `LlmCallDetail` TypeScript type to carry `parsed_input`.

## Non-Goals

- Changing `/http-exchanges` or `/agent-turns` detail layouts.
- Per-SSE-event timeline visualization (current single TTFB/Gen bar is sufficient).
- Rendering multimodal `image` content as an inline image — placeholder `🖼️ image (mime, size)` only. Base64 decoding and image preview is deferred.
- A dedicated "Raw Request Body" / "Raw Response Body" fallback section inside the panel (Raw HTTP drawer is the single raw path).
- Left-side section navigation / anchors inside the drawer — single-column scroll is acceptable.
- Cross-call navigation beyond the existing prev/next chevrons at the top.
- Adding support for wire APIs not already implemented (`anthropic-messages`, `openai-chat`, `openai-responses`).

## Panel Layout

Right-aligned drawer, **70% viewport width** (current 60% → 70%; short of Turn Detail's 92% because content is still single-column), `min-width: 560px`. Existing close/backdrop/prev-next chevron behavior preserved.

Seven vertical sections, top to bottom:

```
┌─────────────────────────────────────────────────────────────────┐
│ LLM Call Detail                                    ▲ ▼   ⓘ   ✕ │
├─────────────────────────────────────────────────────────────────┤
│ ① SUMMARY   [wire/model] [status/finish] [TTFB/E2E] [tokens]   │
│ ② TIMELINE  ⏱ TTFB bar ─── Gen bar ───                         │
│ ③ METADATA  ID · Path · Client · Server · Stream · ...         │
│                                                                 │
│ ④ INPUT  (border-left accent: neutral)                         │
│   ▾ Messages (N)                                               │
│      role chip + preview line per message                       │
│   ▸ System Prompt  (only when top-level system field exists)   │
│   ▸ Tools (N)                                                  │
│   ▸ Sampling · temp=… · max_tokens=… · stream=… · tool_choice= │
│                                                                 │
│ ⑤ OUTPUT  (border-left accent: green — behavior signal)        │
│   ▸ Reasoning                                                  │
│   ▾ Message                                                    │
│   ▾ Tool calls (N)                                             │
│      via shared <CallParsedOutput /> (extracted from CallCard) │
│                                                                 │
│ ─────────────────────────────────────────                       │
│                                     View raw HTTP →             │
└─────────────────────────────────────────────────────────────────┘
```

### ① Summary cards (unchanged)

Four cards, current layout preserved: `wire/model`, `status/finish`, `TTFB/E2E`, `tokens`. No field additions — cache-token breakdown is out of scope (the `CallDetail` query type does not currently expose `cache_read_input_tokens` / `cache_creation_input_tokens`, and surfacing them is a separate task).

### ② Timeline bar (unchanged)

Existing TTFB + Gen horizontal bar and timestamp labels. No per-SSE-event breakdown.

### ③ Metadata grid (unchanged)

The existing `grid-cols-[auto_1fr]` block: `ID`, `Response ID`, `Path`, `Client`, `Server`, `Stream`, `API Type`, `Tenant`. Kept inline because these fields are low-frequency but cheap to skim; no value in hiding behind a popover for this view.

### ④ Input section (new)

Container: vertical stack with a neutral `border-l-2 border-muted-foreground/30` accent and a small `INPUT` label, mirroring the Output container.

**4a · Messages** (expanded by default)

Header line: `▾ Messages (N)` where N is the count from `parsed_input.messages`.

Per message, a single row with:

- Role chip (color per role):
  - `system` — purple
  - `user` — blue
  - `assistant` — green
  - `tool` — orange
- A compact preview of the content:
  - For text content: first ~120 chars of concatenated text blocks.
  - For `assistant` with `tool_use` blocks: message text (if any) + one-line tool-call summary `🔧 name({args_preview})`.
  - For `tool` messages: `⤷ tool_use_id · size ▸ expand`. Long results collapse behind a toggle.
- Click-to-expand gives the full content blocks:
  - `text` → Markdown render (reusing the existing `Markdown` component).
  - `tool_use` → `🔧 name` + full args JSON (pretty-printed) in `<pre>`.
  - `tool_result` → `⤷ result/error` + content in `<pre>` (or Markdown when `kind=text`).
  - `image` → `🖼️ image (mime, size)` placeholder. No base64 render.

Messages whose concatenated content is very long (e.g. a system-style dump in `role=system`) get a per-message `▸ expand (N chars)` toggle.

**4b · System Prompt** (only rendered when the backend surfaces a top-level `system` string)

This exists for the Anthropic Messages wire API, where `system` is a separate top-level field. For OpenAI wire APIs, `system` is just `messages[0]` with `role=system` and should be rendered inside the Messages block — the backend's normalization decides where it lands, not the frontend.

When present: collapsed by default, row header `▸ System Prompt (N chars)`. Expanded: `<pre>` with the full string, Markdown-rendered.

**4c · Tools** (collapsed by default)

Header: `▸ Tools (N)` with an inline teaser listing first 3 tool names, e.g. `— Read, Edit, Bash, +9`.

Expanded: one subsection per tool:

- `name` (bold) + `description` (plain text).
- `input_schema` (JSON Schema) in a collapsible `<pre>` block.

**4d · Sampling** (not collapsed; single line)

Compact key=value list: `temp=1.0 · max_tokens=32000 · top_p=1 · stream=true · tool_choice=auto · stop=[...]`. Unset fields are omitted. The header is just `Sampling`.

### ⑤ Output section

Container: vertical stack with `border-l-2 border-emerald-500/40` accent and an `OUTPUT` label.

Rendering is delegated to a new shared component:

```ts
// components/call-parsed-output.tsx
export function CallParsedOutput({ parsed }: { parsed: ParsedCallContent }) { … }
```

Extracted from the current `CallCard`'s expanded-state inner JSX (the Reasoning / Message / Tool calls block). Both `CallCard` and the new `LlmCallDetailPanel` consume it.

Behavior unchanged from today's CallCard:

- `▸ Reasoning` (collapsed) — `<pre>` with monospaced reasoning text.
- `▾ Message` (expanded) — Markdown render.
- `▾ Tool calls (N)` (expanded when any) — rows with per-tool `args` + paired `result`. Missing results fall through as "(no response, turn ended)".

### ⑥ Raw HTTP link

A single right-aligned text link at the bottom of the scroll area:

```
                                            View raw HTTP →
```

Clicking opens the existing `RawHttpDrawer` over this panel (z-index already handled in `turn-detail/raw-http-drawer.tsx`; the panel reuses it). The drawer keeps showing the same four collapsible sections (Request Headers / Response Headers / Request Body / Response Body) that the current panel currently shows inline — they are not removed from the system, only relocated.

## Backend Changes

### Extended `ParsedInput` (`server/ts-llm/src/model.rs`)

```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedInput {
    // Existing — kept unchanged; consumed by turn_call_enrichment for joiner.
    pub user_message: Option<String>,
    pub tool_results: Vec<ParsedToolResult>,

    // New — consumed only by LLM Call Detail enrichment.
    pub messages: Vec<ParsedMessage>,
    pub system: Option<String>,
    pub tools: Vec<ParsedToolDef>,
    pub sampling: ParsedSampling,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedMessage {
    pub role: ParsedRole,
    pub content: Vec<ParsedContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedRole { System, User, Assistant, Tool }

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedContentBlock {
    Text(String),
    ToolUse { id: String, name: String, args_json: String },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
    Image { mime: Option<String>, size_bytes: Option<u64> },
    /// Forward-compat: unknown content block types are preserved as raw JSON
    /// rather than dropped, so the frontend can render them as
    /// `⚠️ unknown block: {type}` without silently losing payload data.
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolDef {
    pub name: String,
    pub description: Option<String>,
    pub input_schema_json: String, // raw JSON string; frontend pretty-prints
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedSampling {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub stream: Option<bool>,
    pub tool_choice: Option<String>,
    pub stop: Vec<String>,
    pub response_format: Option<String>, // JSON-stringified if structured
}
```

`ParsedInput::default()` covers the "no body / parse failure" case — all fields empty, no exceptional error type surfaced.

### Per-wire-api parsing

**`wire_apis/anthropic.rs`** — Anthropic Messages API

- `system` (top-level string field) → `ParsedInput.system`.
- `messages[]` → `ParsedInput.messages`. Each entry:
  - `role: "user" | "assistant"` → `ParsedRole::User | Assistant`.
  - `content` may be string or array. String → single `ParsedContentBlock::Text`. Array:
    - `type: "text"` → `Text`
    - `type: "tool_use"` → `ToolUse { id, name, args_json: serde_json::to_string(input) }`
    - `type: "tool_result"` → `ToolResult { tool_use_id, content: stringify, is_error: is_error.unwrap_or(false) }`. The `tool_result` lives inside a `user` message in Anthropic, so the parser also re-tags its parent message role from `User` to `Tool` **iff** the message's content is exclusively `tool_result` blocks; mixed-content messages stay `User`.
    - `type: "image"` → `Image { mime: source.media_type, size_bytes: None }` (Anthropic sends base64; we don't measure).
- `tools[]` → `ParsedInput.tools`, mapping `name`, `description`, `input_schema` (serialized back to JSON string).
- `tool_choice` → `sampling.tool_choice` (serialized back; e.g. `"auto"`, `"any"`, `{type:"tool",name:"X"}`).
- `temperature`, `top_p`, `top_k`, `max_tokens`, `stream`, `stop_sequences` → `sampling`.

**`wire_apis/openai.rs`** — OpenAI Chat Completions + Responses

- No top-level `system` field. `ParsedInput.system = None` is correct; the system prompt lives as `messages[0]` with `role: "system"` and the frontend renders it within the Messages block.
- `messages[]` → straightforward role mapping (`system` / `user` / `assistant` / `tool`). `assistant.tool_calls[]` become `ToolUse` blocks.
- `tools[]` → `ParsedInput.tools` (OpenAI: `type=function`, `function.{name,description,parameters}`).
- `tool_choice`, `temperature`, `max_completion_tokens` (Responses API) / `max_tokens` (Chat), `top_p`, `stream`, `stop`, `response_format` → `sampling`.
- OpenAI Responses API (`/v1/responses`) has `input` instead of `messages`. Map single-string input to a single `User` message with one `Text` block; array input maps like Chat messages. `instructions` (Responses API's system equivalent) → `ParsedInput.system`.

### API payload

`/api/llm-calls/{id}` (`turn_call_enrichment::enrich_single`) changes:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct EnrichedCallDetail {
    #[serde(flatten)]
    pub base: CallDetail,
    pub parsed: ParsedCallContent,      // output — unchanged
    pub parsed_input: ParsedInputView,  // new
}
```

`ParsedInputView` serializes `ParsedInput` to the wire with lowercase role strings and `{type, ...}` tagged blocks. In practice this is achieved via serde attributes on the existing types:

- `ParsedRole`: `#[serde(rename_all = "snake_case")]` yielding `"system" | "user" | "assistant" | "tool"`.
- `ParsedContentBlock`: `#[serde(tag = "type", rename_all = "snake_case")]` yielding `{"type":"text","text":"..."}`, `{"type":"tool_use",...}`, etc. The `Unknown(serde_json::Value)` variant is flattened so it emits whatever the original block was (preserving its `type` field as-is).

Existing `parsed_out` / `next_in` joiner logic is untouched because this enrichment path never feeds the turn joiner — only the standalone LLM Call Detail path consumes `parsed_input`.

The turn-list `enrich` function keeps using only `user_message` / `tool_results` and continues to ignore the new fields (parsing them is cheap; we just don't serialize them at the turn-list level to avoid bloating that payload).

### TypeScript type additions (`console/src/types/api.ts`)

```ts
export type ParsedRole = "system" | "user" | "assistant" | "tool"

export type ParsedContentBlock =
  | { type: "text"; text: string }
  | { type: "tool_use"; id: string; name: string; args_json: string }
  | { type: "tool_result"; tool_use_id: string; content: string; is_error: boolean }
  | { type: "image"; mime: string | null; size_bytes: number | null }

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

export interface LlmCallDetail {
  // … existing fields …
  parsed: ParsedCallContent
  parsed_input: ParsedInput  // NEW
}
```

## Frontend Component Layout

```
pages/
  llm-call-detail-panel.tsx           (rewritten)

components/
  call-parsed-output.tsx              (NEW — extracted from call-card.tsx)
  llm-call-detail/
    summary-cards.tsx                 (NEW — current SummaryCard inlined, lifted)
    timeline-bar.tsx                  (NEW — current TimelineBar lifted)
    metadata-grid.tsx                 (NEW — current MetadataGrid lifted)
    input-section.tsx                 (NEW)
      messages-block.tsx              (NEW)
      system-block.tsx                (NEW)
      tools-block.tsx                 (NEW)
      sampling-block.tsx              (NEW)

  turn-detail/
    call-card.tsx                     (MODIFIED — replaces its inner output JSX with <CallParsedOutput />)
```

`RawHttpDrawer` is already a shared component under `components/turn-detail/` — kept as-is. If the location feels wrong once the LLM Call Detail also uses it, move it to `components/raw-http-drawer/` in a follow-up; not part of this spec.

## Error & Edge States

- **Request body not captured** (`request_body === null`): Input section body renders a single gray "Request body not captured" row. Output section behaves as today (Output still rendered from response body).
- **Request body present but not parseable as JSON** or **parses but doesn't match any wire API shape**: `parsed_input` returned as `default()` (empty messages, empty tools, empty sampling, null system). Input section renders "Could not parse request body as `{wire_api}`" with a "View raw HTTP →" shortcut inline. This makes the panel's primary text-level failure legible without drilling into the drawer.
- **Multimodal image without mime**: render `🖼️ image` without size info; do not crash.
- **Unknown content block types** (forward-compat with provider schema changes): render as `⚠️ unknown block: {type}` plus the raw JSON under a details element. Parser should preserve unknown blocks rather than dropping them — add an `Unknown(serde_json::Value)` variant to `ParsedContentBlock` to make this explicit.
- **Sampling all-unset** (rare — typically `stream`/`max_tokens` is present): render as `Sampling · (defaults)`.
- **Output side unchanged behaviors**: existing CallCard fallbacks (missing reasoning / missing message / empty tool_calls) are inherited via the shared `CallParsedOutput` component.

## Testing

Rust (`ts-llm`):

- Per wire API, unit tests covering: messages with text-only content, messages with `tool_use`, messages with `tool_result` (role re-tag to `Tool`), tools definition extraction, sampling extraction, image content block preservation, unknown content block preservation. Existing `parse_input` tests in `wire_apis/anthropic.rs` and `wire_apis/openai.rs` are extended, not replaced.
- Enrichment test in `routes/turn_call_enrichment::enrich_single` verifying `parsed_input` is populated when `request_body` is present and empty when absent.

Rust (`ts-api`):

- Integration test hitting `/api/llm-calls/{id}` and asserting `parsed_input.messages[0].role == "user"` (OpenAI fixture) and `parsed_input.system == Some("…")` (Anthropic fixture).

TypeScript (console):

- Component test for `<MessagesBlock />` rendering each role chip, collapsing long tool_result, and expanding individual messages.
- Component test for `<ToolsBlock />` teaser vs expanded view.
- Refactor sanity: existing CallCard tests (if any) keep passing after the output JSX is lifted into `CallParsedOutput`; any new CallCard-specific behavior tests are unchanged.

Manual verification (per primary wire API):

- Anthropic tool-use call: Input shows Messages + System Prompt + Tools + Sampling; Output shows Message + Tool calls block with paired results (where `next_call_request_body` exists).
- OpenAI Chat call: Input Messages starts with `role=system`, no separate System Prompt block.
- OpenAI Responses call: `instructions` → System Prompt, `input` → Messages.
- Streaming vs non-streaming: Timeline bar renders; Output parsing works off the reassembled response body.

## Implementation Order

1. **Rust — extend `ParsedInput` struct** with new fields (no parser changes yet). Existing consumers continue to compile. Add `ParsedInputView` serialization shape.
2. **Rust — implement new fields** in `anthropic.rs` `parse_input`. Unit tests.
3. **Rust — implement new fields** in `openai.rs` `parse_input` (Chat + Responses). Unit tests.
4. **Rust — `enrich_single`** adds `parsed_input` to the payload. API integration test.
5. **Frontend — add `ParsedInput` types** to `types/api.ts`. Confirm existing TS compiles.
6. **Frontend — extract `CallParsedOutput`** shared component out of `call-card.tsx`. Verify Turn Detail still works (no visual diff).
7. **Frontend — build Input section components** (`MessagesBlock` → `SystemBlock` → `ToolsBlock` → `SamplingBlock`) one at a time.
8. **Frontend — rewrite `LlmCallDetailPanel`** composing summary + timeline + metadata + input + output + raw-http link. Remove the old four Raw collapsibles from this panel.
9. **Manual pass** on a real capture covering Anthropic + OpenAI Chat + OpenAI Responses.

Each step is independently shippable and revertible; no step leaves the UI in a half-built intermediate state because the old panel stays in place until step 8.
