# Agent Turn Detail — Redesign

## Context

The current `AgentTurnDetailPanel` (`console/src/pages/agent-turn-detail-panel.tsx`) exposes a lot of cold debugging metadata but little of the actual agent behavior. Users open a turn and see Turn ID / Session ID / Tenant first, with `User Input`, `Final Answer`, and `Call Timeline` buried behind three tabs. The left sidebar repeats 6 fields per call, competing with the main content. There's no way to see *what the agent did* — which tools it called, what results came back, what reasoning the model produced.

This redesign re-frames the page around the **agent-behavior narrative**: user question → interleaved tool calls / reasoning / text → final answer. Debug-grade raw HTTP becomes a secondary drill-down.

## Target User

**Primary:** Product / ops / QA reading a turn to understand what the agent did.

Not primary: SRE doing HTTP-level debugging. They remain supported via a secondary "Raw HTTP" drawer, but the default view is tuned for the primary reader.

## Scope

- Redesign `AgentTurnDetailPanel` layout and interactions.
- Extend the `/api/agent-turns/{id}/calls` and `/api/llm-calls/{id}` payloads to surface parsed call content (tool calls, tool results, reasoning, message text).
- Add per-wire-api parsers in `ts-llm` and a turn-scoped tool-use ↔ tool-result joiner in `ts-turn`.
- Keep existing `AgentTurnDetail.user_input` / `.final_answer` extraction unchanged.

## Non-Goals

- Cross-turn navigation / session view.
- Editing or replaying turns.
- Metrics aggregation changes.
- Support for wire APIs beyond the three currently supported (`anthropic-messages`, `openai-chat`, `openai-responses`).

## Overall Layout

Two-column panel (unchanged outer frame: right-aligned drawer, 92% viewport width, existing close/backdrop behavior).

```
┌─────────────────────────────────────────────────────────────────────┐
│ Agent Turn Detail            codex-cli · timmy-1234 · 019d…13b  ⓘ ✕ │
├──────────┬──────────────────────────────────────────────────────────┤
│          │  ┌──────────┬───────────┬──────────┬──────────┐          │
│ Timeline │  │ Calls 23 │ In 1.8M   │ Duration │ Status   │          │
│ 7m 31s   │  │ 🔧18 💬4 │ Out 6.6K  │ 7m 31s   │ ✓        │          │
│          │  │    🎯1   │ $0.12     │ slow #7  │ stop     │          │
│ 1  🔧 ▃  │  └──────────┴───────────┴──────────┴──────────┘          │
│ 2  🔧 ▃  │                                                          │
│ 3  💬 ▁  │  👤 User                                                 │
│ 4  🔧 ▁  │     审查当前 ts-storage 模块，是否存在问题...            │
│ 5  🔧 ▂  │                                                          │
│ 6  🔧 ▂  │  #1 🔧 read_file · 2.1s · 50.3K↑ 314↓         ▸         │
│ 7  🔧 ▇⚠ │     "让我先看一下 lib.rs 的结构..."                      │
│ …        │  ...                                                     │
│ 22 💬 ▂  │  #7 🔧 shell · ⚠ 27s                          ▸         │
│ 23 🎯 ▃  │     cargo check --package ts-storage                     │
│          │  ...                                                     │
│          │  #23 🎯 final · 5.8s                           ▸         │
│          │     ts-storage 存在以下问题：1. write buffer...          │
│          │                                                          │
│          │  🎯 Final Answer                                         │
│          │     (full markdown render of final_answer)               │
└──────────┴──────────────────────────────────────────────────────────┘
   140px                             rest
```

Structural deltas from current:

- Tabs removed. `User Input` / `Final Answer` become inline cards bookending the call narrative. `Call Timeline` (Gantt) is absorbed into the left nav.
- Left sidebar is no longer a detail-duplicating `CallCard` list. It becomes a vertical Gantt mini-map (time navigation).
- Cold metadata (Turn ID / Session ID / Wire API / etc.) moves out of the body to an `ⓘ` popover in the header.
- Clicking a call expands it inline rather than replacing the main panel. Raw HTTP is a separate right-side drawer.

## Top Bar

Single 40px row.

```
Agent Turn Detail          codex-cli · timmy-1234 · 019dae9b…e1013b  ⓘ  ✕
```

- Left: fixed title.
- Right: `agent_kind · tenant_id · turn_id` (turn_id truncated as `first8…last6`, tooltip shows full), then `ⓘ` button, then `✕`.
- `ⓘ` opens a popover showing cold metadata: full Turn ID, Session ID, Start, End, Wire API, Models, Subagents. Nothing here is referenced during normal reading.
- `✕` closes the panel.

The existing left-sidebar "Agent Turn + turn_id" header block is removed; its function is replaced by the top bar + `ⓘ` popover.

## Stats Cards

4 compact cards (~58px tall), grid-cols-4.

1. **Calls** — primary: `call_count`. Secondary row: type breakdown `🔧 tool · 💬 text · 🎯 final`. Icon counts come from parsed `type` field (Phase 2+); before parsing is available, secondary row is omitted.
2. **Tokens** — primary: `in {total_input_tokens} / out {total_output_tokens}` in two sub-columns. Secondary row: `$total_cost_usd` when non-null; hidden otherwise.
3. **Duration** — primary: `formatDuration(duration_ms)`. Secondary row: `slowest #{N} {ms}` (click to scroll-and-highlight that call in the nav). Slowest = max `e2e_latency_ms` across calls.
4. **Status** — `TurnStatusBadge` + `finish_reason` (via `FinishBadge`).

No separate anomaly banner. Slow / error signals are carried entirely on per-call rows (nav + cards).

## Left Nav — Vertical Gantt (140px, sticky)

### Header

Two lines: `Timeline` and `formatDuration(duration_ms)`. No turn ID, no agent_kind.

### Row Anatomy

Each call is a row in a 4-column grid:

```
grid-template-columns: 16px 16px 1fr 36px;
```

- **Seq** — `#{sequence}`, right-aligned, tabular-nums.
- **Type icon** — lucide `Wrench` / `MessageSquare` / `Target`, 14px. Determined by parsed `type`; before parsing is available (Phase 1), all rows use a neutral dot.
- **Bar** — a shared-time-axis bar inside a full-width track:
  - Left offset: `(call.request_time - turn.start_time) / turn.duration_ms`
  - Width: `(call_end - call.request_time) / turn.duration_ms` where `call_end = call.complete_time ?? call.response_time ?? call.request_time`
  - Inner fill: TTFB amber segment + gen blue segment. If bar width < 3% of track, collapse to single blue fill.
- **Duration** — `formatMs(e2e_latency_ms)`, right-aligned, tabular-nums.

### State Semantics

| State | Bar fill | Row left border | Duration color |
|-------|----------|-----------------|----------------|
| Normal | amber + blue | none | default |
| Slow (`e2e_latency_ms > 10000`, configurable) | solid amber | `border-l-2 border-amber-500/70` | amber |
| Error (`status_code >= 400 \|\| finish_reason ∈ {error, truncated}`) | solid red | `border-l-2 border-red-500/70` | red |
| Final call (last, `type = final`) | amber + emerald | none | default |
| Active (current-scroll-position call) | background `bg-blue-50` | — | — |

### Interactions

- `hover` — row background `bg-muted/50`. Tooltip: `model · tokens in/out · TTFB/E2E`.
- `click` — smooth-scroll to right-pane call card; flash a blue ring for 600ms.
- `dblclick` — expand that call inline as well.
- Scrolling in the right pane updates active row via `IntersectionObserver` on call cards.

### Edge Cases

- 0 calls → "No calls" centered.
- >80 calls → enable `@tanstack/react-virtual` (same library as elsewhere). Below threshold: render directly.

## Main Pane — Narrative

Vertical scroll. In order:

1. Stats cards (see above).
2. `👤 User` card — if `user_input` non-null.
3. Sequence of call cards in sequence order.
4. `🎯 Final Answer` card — if `final_answer` non-null.

### User Input Card

- Header: `👤 User`, right-aligned request_time of `user_call_id`.
- Body: markdown render of `user_input`.
- Background: `bg-blue-50/60` (dark variant: `bg-blue-950/30`).
- Long content: show first ~8 lines (measured via `max-h-[240px] overflow-hidden`), with `Show more ▾` revealing full content. Threshold inline; no config.

### Call Card — Collapsed (default state)

Two-row grid. Row 1 is always present; Row 2 is omitted if no preview text.

```
Row 1: [#seq] [type-icon] [label chips]  ...spacer...  [dur] [tokens in↑ out↓] [▸]
Row 2: [                   preview (60 chars, 1 line, truncate)                     ]
```

- **Label chips:**
  - `type = tool_call` → tool names chip: `{tool_calls[0].name}[, {tool_calls[1].name}][, +N more]`. Max 2 names shown; rest collapse to `+N more`.
  - `type = text` → single "text" chip.
  - `type = final` → single "final" chip (emerald).
- **preview:**
  - `type = tool_call` → first 60 chars of `message_preview` if present, else first 60 chars of primary tool's `args_preview`.
  - `type = text` or `final` → first 60 chars of `message_preview`.
- **Anomaly styling** mirrors the nav: slow → amber left border + amber duration; error → red left border + red duration + `✗` prefix on duration.
- Entire card click → toggle expansion. Anchor: `id="call-{sequence}"`.

### Call Card — Expanded

The card header row stays identical (only the chevron flips `▾`). Below it appears a reveal area with up to four subsections, each rendered only if its data is non-empty, each independently collapsible.

1. **Reasoning** (`has_reasoning == true`) — default collapsed. Expanded: full reasoning text in a `max-h-[600px] overflow-auto` container. Header shows `Reasoning ({token_count_estimate} tokens)` if available.
2. **Message** (`parsed.message != null`) — default expanded. Full message text, markdown-rendered. If >20 lines, wrap in `max-h-[400px] overflow-auto`.
3. **Tool calls** (`parsed.tool_calls.length > 0`) — default expanded. One row per tool_use:
   - Top: tool name + per-tool wall-clock (from parser if available; else omitted).
   - Args: formatted JSON in a code block. Default-expanded; if >20 lines, collapse to 20 with `Show more`.
   - `⤷ result · {size} · {lines|matches|items}` line below the args. Default collapsed. Expanded: the full result content as a code block (same >20-line truncation).
   - Error-result styling: `⤷ error · …` in red, result content with red-tinted code block.
   - Missing-result (last call, no successor): `⤷ result · (no response, turn ended)` in grey, non-clickable.
4. **Meta row** — single grey line at bottom: `{model} · {wire_api} · TTFB {ms} · finish: {reason}`.
5. **`View raw HTTP →`** link below the meta row. Opens the Raw HTTP drawer.

### Final Answer Card

- Header: `🎯 Final Answer`, right: `#{final_call_id.sequence} · {duration}`; clicking the `#N` scrolls the corresponding call card into view and expands it.
- Body: full markdown render of `final_answer`. No truncation.
- Background: `bg-emerald-50/60` (dark: `bg-emerald-950/30`).
- If `final_answer == null` but the turn ended, render a single grey line under the last call card: `Turn ended without a final answer`. No card.

## Raw HTTP Drawer

Right-side secondary drawer, opened from any expanded call's `View raw HTTP →`.

- Width: `min(720px, 50vw)`.
- Slides in from the right, overlays the main panel (main panel remains scrollable and interactive — users can open multiple drawers sequentially by clicking different `View raw HTTP →` links without returning to the panel first).
- No backdrop; closing is via an explicit `✕` on the drawer header, or by clicking another `View raw HTTP →` which replaces content.
- Content is the bottom half of today's `LlmCallDetailView`: 4-card stats (wire_api/model, status/finish, TTFB/E2E, tokens), `CallTimelineBar`, metadata grid, and the four `CollapsibleSection`s (Request Headers, Response Headers, Request Body, Response Body).
- Closing does not affect main-panel state (scroll position, which calls are expanded).

## URL / Deep Linking

- `/agent-turns/{id}` — default.
- `/agent-turns/{id}?call={sequence}` — auto-expand that call and scroll to its card on mount.
- `/agent-turns/{id}?call={sequence}&raw=1` — also open the Raw HTTP drawer for that call.
- Implemented with `useSearchParams`. Expanding/collapsing a call or opening/closing the drawer updates the URL (replace, not push, so Back doesn't accumulate intermediate states).

## Loading, Error, Empty States

| Condition | Presentation |
|-----------|--------------|
| `useAgentTurnDetail` loading, no cache | Panel-center `Loader2` spinner |
| `useAgentTurnDetail` error | "Failed to load agent turn detail" + close button (existing behavior) |
| `useAgentTurnCalls` loading, turn loaded | Nav shows 4 skeleton rows; main pane shows 3 skeleton call cards. Stats / user input render immediately. |
| `calls.length == 0` | Nav: "No calls" centered. Main pane: User + Final cards render if present. |
| `user_input == null` | User card omitted. |
| `final_answer == null` | Final card omitted, "Turn ended without a final answer" grey line after last call card. |
| Per-call parsed fetch fails (Phase 2+) | Expanded area shows "Failed to parse call · View raw HTTP →" |

## Keyboard

- `Esc` — close drawer if open; else close panel.
- `↑` / `↓` — move "focused call" up/down. Focused call is highlighted in the nav; scrolls into view if needed.
- `Enter` — toggle expansion of the focused call.
- (Not in MVP) `Cmd/Ctrl+K` — focus a "jump to call #" search input.

## Backend Changes

### Parser Module (`ts-llm/src/parse/`)

New module with a trait + three implementations, dispatched by `wire_api`:

```rust
pub trait CallParser {
    fn parse_output(body: &str) -> anyhow::Result<ParsedOutput>;
    fn parse_input(body: &str)  -> anyhow::Result<ParsedInput>;
}

pub struct ParsedOutput {
    pub reasoning: Option<String>,
    pub message: Option<String>,
    pub tool_calls: Vec<ParsedToolCall>,
}

pub struct ParsedInput {
    pub user_message: Option<String>,          // first user-role message
    pub tool_results: Vec<ParsedToolResult>,   // indexed by tool_use_id
}

pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub args_json: String,      // canonical JSON
}

pub struct ParsedToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}
```

Files: `mod.rs` (trait + dispatcher), `anthropic.rs`, `openai_chat.rs`, `openai_responses.rs`.

Dispatch by `wire_api` string; return an error for unknown wire_api (API layer decides how to degrade).

### Cross-Call Join (`ts-turn`)

Given a turn's calls sorted by `sequence`, walk pairs and attach results:

```rust
fn attach_tool_results(calls: &mut [EnrichedCall]) {
    for i in 0..calls.len() {
        let next_input = calls.get(i + 1).map(|c| &c.parsed_input);
        for tc in &mut calls[i].parsed_output.tool_calls {
            tc.result = next_input
                .and_then(|ni| ni.tool_results.iter().find(|tr| tr.tool_use_id == tc.id))
                .cloned();
        }
    }
}
```

The last call's tool_use blocks have no result (no successor call). This is not an error — the UI renders `(no response, turn ended)`.

### API Payload Extensions

**`GET /api/agent-turns/{id}/calls`** — extend each item:

```ts
interface AgentTurnCallItem {
  // existing fields unchanged
  type: "tool_call" | "text" | "final"
  tool_calls: Array<{
    id: string
    name: string
    args_preview: string       // first 200 chars
    result_summary: {
      size_bytes: number
      kind: "text" | "json" | "error" | "binary" | "missing"
      is_error: boolean
    } | null                   // null when successor call doesn't exist
  }>
  has_reasoning: boolean
  reasoning_preview: string | null   // first 120 chars
  message_preview: string | null     // first 60 chars
}
```

Determination of `type` (first match wins):
- `call.id == turn.final_call_id` → `final`
- `tool_calls.length > 0` → `tool_call`
- else → `text`

Precedence puts `final` first so that a final call that also emitted tool calls (an abnormal end) still renders as the narrative's terminus.

Parsing is done on-read at the API layer. A 23-call turn with ~1.8M input tokens (~7MB of JSON) parses in <250ms in aggregate; this endpoint is cold (only hit when detail panel opens), so no caching is added in MVP.

**`GET /api/llm-calls/{id}`** — add a `parsed` field alongside existing body/headers:

```ts
interface LlmCallDetail {
  // existing fields unchanged
  parsed: {
    reasoning: string | null
    message: string | null
    tool_calls: Array<{
      id: string
      name: string
      args_json: string             // full
      result: {
        content: string             // full
        size_bytes: number
        kind: "text" | "json" | "error" | "binary" | "missing"
        is_error: boolean
      } | null
    }>
  }
}
```

Note: `result` on a single-call detail endpoint requires access to the *next* call's body. Resolution: the detail endpoint accepts an optional `turn_id` query param (or infers it from the call's `turn_id` column), fetches the immediate successor call's body, and runs the same join logic. If the call is the last in its turn, `result` is null.

### Relationship to Existing Fields

`AgentTurnDetail.user_input` and `.final_answer` are already extracted during turn aggregation via agent-kind profiles. This redesign does not change that pipeline. The new per-call parser is independent and finer-grained. As a conceptual relationship (not enforced by code):

- `user_input` ≈ `parsed_input.user_message` on the call referenced by `user_call_id`.
- `final_answer` ≈ `parsed_output.message` on the call referenced by `final_call_id` (subject to agent-specific post-processing).

Unifying the two paths is out of scope.

## Phasing

Three independently-shippable phases.

### Phase 1 — Frontend Refactor (backend unchanged)

Ships all layout changes without new data.

- New `AgentTurnDetailPanel` structure: top bar, stats cards, vertical Gantt nav, narrative pane, Raw HTTP drawer, `ⓘ` metadata popover, URL sync, keyboard shortcuts.
- Call cards render with available fields only: sequence, finish_reason (via existing `FinishBadge` as a fallback type signal), duration, tokens. No tool names, no text previews.
- Expanded call card in Phase 1 shows only the meta row + `View raw HTTP →` link — no Reasoning / Message / Tool-calls subsections (no parsed data yet). The affordance is in place; the content fills in at Phase 2/3.
- `View raw HTTP →` opens the drawer with existing `LlmCallDetail` fields.
- Stats "Calls" card omits the type breakdown secondary row.

Visible improvement: the current pain (cold metadata first, buried narrative, duplicated sidebar) is resolved. The page is immediately usable even if phases 2–3 are deferred indefinitely.

### Phase 2 — Per-Wire-API Parsers + List Enrichment

- Implement three `CallParser` impls.
- Extend `/api/agent-turns/{id}/calls` payload with `type`, `tool_calls[]`, `has_reasoning`, `reasoning_preview`, `message_preview`.
- Update frontend to render real type icons, tool-name chips, text previews. Turn on the type-breakdown row in the `Calls` stats card.

No new UI structures — only content inside existing placeholders comes alive.

### Phase 3 — Full Parsed Detail + Tool-Result Join

- Extend `/api/llm-calls/{id}` with the `parsed` field.
- Implement `attach_tool_results` in `ts-turn`; ensure detail endpoint can join with the successor call.
- Enable the expanded call card's Reasoning / Message / Tool-calls / Result subsections.

After Phase 3, the narrative is complete: users can follow the whole agent loop — question → tool call → result → tool call → result → ... → final answer — without leaving the page.

## Testing Strategy

- **Parsers (Phase 2):** unit tests per wire_api, fixtures drawn from real captured bodies in `server/ts-llm/tests/fixtures/`. Cover: empty output, text-only output, tool-only output, mixed output, reasoning blocks, streaming-assembled bodies (final reconstructed JSON only; parser consumes that).
- **Join (Phase 3):** unit tests in `ts-turn` covering: all tool_uses matched; orphan tool_use in last call; out-of-order sequences (shouldn't happen but guard against); multiple tool_uses in one call all matched in next.
- **Frontend (Phase 1):** component tests for the new panel — render with full data, with missing user_input, with missing final_answer, with 0 calls, with slow/error calls. Snapshot the vertical Gantt with a known dataset to lock visual encoding.
- **Integration:** end-to-end test against a known captured turn: open the panel, verify nav rows, click through to expand, verify Raw HTTP drawer opens.

## Open Questions

None at time of writing. Known deferrals:

- Virtualization threshold for nav (>80 calls) — implement when first seen.
- Unifying turn-level extraction with per-call parser — Phase 4+, not in this spec.
- Slow threshold (`10000ms`) — hardcoded constant; revisit if users report false positives/negatives.
