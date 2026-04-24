# Turn Detail · Narrative Fusion + Capture-Aware Tool Index

## Context

`AgentTurnDetailPanel` (`console/src/pages/agent-turn-detail-panel.tsx`) was redesigned in [2026-04-21-turn-detail-redesign-design.md](./2026-04-21-turn-detail-redesign-design.md), which introduced the top bar, stats cards, vertical Gantt nav, inline call cards, and the `View raw HTTP →` drawer. That redesign landed the overall shell but inherited three structural inconsistencies in the narrative pane itself:

1. **User Input is separated from Call#1** — rendered as its own `UserCard`, even though the user message is literally the request body of Call#1. The reader sees two UI blocks for one HTTP fact.
2. **Tool results are merged into the wrong call** — `buildResultLookup` ([anthropic.tsx:476](../../../console/src/components/call-renderers/anthropic.tsx#L476)) stitches Call#N+1's tool_result blocks into Call#N's display. This obscures "result arrived in a later request" and relies on the unstated assumption that resolution always happens in N+1.
3. **Final Answer is shown twice** — both inside the last call's expanded Output and again as a standalone `FinalAnswerCard` below it.

The root cause is the same for all three: the page mixes two boundary models — **LLM Call (HTTP fact)** and **Agent Step (narrative)** — and applies them inconsistently. This spec commits to **Call-first** for the entire narrative pane, fuses the User/Final bookends back into their source calls, and replaces the N+1 result lookup with a **turn-scoped `tool_use_id` index** that is aware of packet-capture loss.

## Target User

Same as the prior spec: product / ops / QA reading a turn to understand what the agent did. No change.

## Decision Summary

| Area | Decision |
|---|---|
| Main narrative unit | LLM Call (fact-first, matches TokenScope's API-monitoring positioning) |
| `UserCard` component | **Removed.** User message renders inside Call#1's Input subsection. |
| `FinalAnswerCard` component | **Removed.** Final answer renders inside Call#last's Output subsection with emerald styling. |
| Call card expanded layout | Two clearly-labelled subsections: `Input · request body` and `Output · response body`. |
| Tool linking | Bidirectional, id-based pointers (scheme A1). No summary strip (scheme A2′ rejected as redundant). |
| Tool index scope | Turn-scoped `Map<tool_use_id, {origin, resolution}>`. Replaces N+1 assumption. |
| Capture-loss awareness | Four pointer states distinguish healthy, legit pending, mid-turn capture gap, and orphan tool_result. |
| Stats card 4 | `⚠ Unresolved N` when capture gaps exist; falls back to `Status` when `N == 0`. |

## Scope

- Rewrite `TurnDetailView` in `agent-turn-detail-panel.tsx` (remove `UserCard`, `FinalAnswerCard`, restructure `CallCard` expanded region).
- Rewrite `buildResultLookup` in `anthropic.tsx` (and equivalent logic in other wire-api renderers) to a turn-scoped index.
- Extend call renderers' Output/Input views to display the four pointer states.
- Update the `Stats` cards to conditionally render `⚠ Unresolved` in place of `Status`.

## Non-Goals

- Cross-turn or session-level navigation.
- Editing or replaying turns.
- Changes to metrics aggregation or the storage layer.
- Support for wire APIs beyond the three currently implemented (`anthropic`, `openai-chat`, `openai-responses`).
- Backend-side parsing or pre-computation of the tool index. The map is built client-side from the bodies already returned by `/api/agent-turns/{id}/calls`.
- Overlay-specific rendering changes (e.g., `ClaudeCliOverlay` message/tool-result customizations remain).

## Data Model — Turn-Scoped Tool Index

```ts
interface ToolOrigin {
  call_sequence: number
  call_id: string
  tool_name: string           // for back-link label
}

interface ToolResolution {
  call_sequence: number
  call_id: string
  is_error: boolean
  size_bytes: number
  content: string             // full text, either string content or JSON-stringified blocks
}

interface ToolIndexEntry {
  origin: ToolOrigin | null       // null = orphan tool_result (origin call missed)
  resolution: ToolResolution | null // null = unresolved (see state rules below)
}

type ToolIndex = Map<string /* tool_use_id */, ToolIndexEntry>
```

### Build Algorithm

Single pass over `calls` sorted by `sequence`, two sub-walks per call.

```ts
function buildToolIndex(calls: AgentTurnCallItem[]): ToolIndex {
  const index: ToolIndex = new Map()

  // Pass 1 — collect tool_use origins from each call's response body.
  for (const call of calls) {
    for (const block of iterToolUseBlocks(call.wire_api, call.response_body)) {
      index.set(block.id, {
        origin: { call_sequence: call.sequence, call_id: call.id, tool_name: block.name },
        resolution: null,
      })
    }
  }

  // Pass 2 — collect tool_result resolutions from each call's request body.
  for (const call of calls) {
    for (const block of iterToolResultBlocks(call.wire_api, call.request_body)) {
      const entry = index.get(block.tool_use_id) ?? { origin: null, resolution: null }
      entry.resolution = {
        call_sequence: call.sequence,
        call_id: call.id,
        is_error: block.is_error,
        size_bytes: byteLength(block.content),
        content: block.content,
      }
      index.set(block.tool_use_id, entry)
    }
  }

  return index
}
```

`iterToolUseBlocks` and `iterToolResultBlocks` dispatch by `wire_api` and are per-provider (Anthropic messages, OpenAI chat `tool_calls` / tool-role messages, OpenAI responses `function_call` / `function_call_output` items).

The index is memoized at the panel level via `useMemo` keyed on `calls` reference identity; TanStack Query keeps `calls` stable across rerenders.

### Replacement for `buildResultLookup`

The N+1 walker is deleted. Callers that need "what's the result for this `tool_use_id`" now consult the turn-scoped index. Passing `nextCallRequestBody` through the renderer tree is no longer needed (the index handles cross-call lookup globally).

## Pointer State Rules

For a `tool_use` block rendered inside a call card, look up `index.get(tool_use_id)`:

| Condition | Rendered pointer | Classification |
|---|---|---|
| `resolution != null` | `→ result in #M ✓` · clickable jump | **Healthy** |
| `resolution == null` AND call is final (`call.id == turn.final_call_id`) AND `turn.end_time != null` AND `turn.finish_reason` is a normal terminator (`end_turn`, `stop`, `max_tokens`, `stop_sequence`) | `→ no response (turn ended)` · grey, non-clickable | **Legit pending** |
| `resolution == null` AND above conditions not all met (i.e., there are subsequent calls, or turn didn't normally terminate) | `⚠ → result not captured` · amber | **Mid-turn capture gap** |

For a `tool_result` block rendered inside a call card's Input subsection, look up by `tool_use_id`:

| Condition | Rendered back-pointer | Classification |
|---|---|---|
| `origin != null` | `← from #N · {tool_name}` · clickable jump | **Healthy** |
| `origin == null` | `⚠ ← origin not captured` · amber, non-clickable | **Orphan result** |

**Jump behavior:** clicking a pointer scrolls the target call into view and briefly flashes a blue ring (same 600ms highlight the existing nav-click uses). The target call is not forced open — if it's collapsed, it stays collapsed; the user can expand if needed. This preserves scroll context for agents with 50+ calls.

## UI Layout

### Overall Structure

Unchanged from the prior spec: right-side drawer, two-column body (Gantt nav + main pane), top bar with `ⓘ` popover and `✕`. This spec only changes the main pane's narrative structure.

### Main Pane — Narrative

Vertical scroll. Only two kinds of blocks exist:

1. Stats cards (4 cards, grid-cols-4 — see Stats section below)
2. Call cards in sequence order

**No `UserCard`. No `FinalAnswerCard`.** Both components are deleted; their files are removed from `console/src/components/turn-detail/`.

### Call Card — Collapsed

Header row is unchanged structurally from the prior spec, but with the following refinements:

- **Call#1** gets a `👤 user` chip prepended to its type chips, signalling "this call is where the user turn begins." All other calls omit this chip.
- **Call#last** (`call.id == turn.final_call_id`) gets a `🎯 final` chip; when present, it replaces any `🔧 tools` chip (emerald takes precedence, matching the prior spec's `type` precedence).

Preview row (60-char single-line truncate) is unchanged. For Call#1 the preview reads from `user_input` (falling back to `response_body` message preview if `user_input` is null). For Call#last the preview reads from `final_answer`.

### Call Card — Expanded

The expanded region is split into two visually distinct subsections, each with a 2px left border and a small uppercase label:

```
├─ grey left border ─── Input · request body ──────────┐
│                                                       │
│  (content: see per-call-type rules below)             │
├─ emerald left border ─ Output · response body ───────┤
│                                                       │
│  (content: see per-call-type rules below)             │
└──────────────────────────────────────────────────────┘
  meta row: model · wire_api · TTFB · finish
  View raw HTTP →
```

#### Input subsection content

- **Call#1 (first call in turn):** render `user_input` as a blue-tinted user-message card (same styling the deleted `UserCard` used: `bg-blue-50/60` light, `bg-blue-950/30` dark). Markdown-rendered. Long content (>8 lines) gets a `Show more ▾` toggle with `max-h-[240px] overflow-hidden` collapsed state.
- **Non-first call:** render the sequence of `tool_result` blocks found in the request body. Each result is a grey card with:
  - Header row: `⤷ tool_result` label, `tool_use_id` monospace, size, `← from #N · {tool_name}` back-pointer.
  - Body: full content in a `max-h-[240px] overflow-auto` `<pre>`.
  - Error styling: red-tinted background + red text when `is_error`.
  - Orphan styling: amber-tinted background + `⚠ ← origin not captured` back-pointer (no click target).
  - If the call also has non-tool-result content in its request (rare — assistant-role messages get carried forward in some wire APIs), those are rendered after the tool_results as neutral message cards.

#### Output subsection content

- **Call#last (final call):** render `final_answer` as an emerald-tinted `final-answer` card (same styling the deleted `FinalAnswerCard` used: `bg-emerald-50/60` light, `bg-emerald-950/30` dark). Markdown-rendered, no truncation. If the final call also emitted `tool_use` blocks (the "abnormal end" case the prior spec calls out), render them after the final-answer card with the normal tool-use styling; each such tool_use will display a `→ no response (turn ended)` legit-pending pointer (per the state rules above).
- **Non-final call:** render the sequence of response blocks in order:
  - `thinking` blocks: purple-tinted italic card.
  - `text` blocks: neutral assistant-message card, markdown-rendered.
  - `tool_use` blocks: amber-tinted card with name, id, formatted args JSON, and the `→ result in #M ✓` / `→ no response (turn ended)` / `⚠ → result not captured` pointer per state rules.

#### Meta row

Unchanged: `{model} · {wire_api} · TTFB {ms} · finish: {reason}`, grey monospace.

#### Raw HTTP link

Unchanged: `View raw HTTP →` opens the existing drawer.

### Rejected: A2′ Tools-Raised Strip

Considered and rejected. A per-call summary strip listing "tools raised by this call" duplicates the same `→ result in #M` pointer that already lives on each `tool_use` block. The collapsed-state header chip (`🔧 N tools`) already provides the count-at-a-glance view for closed cards. No additional strip is added.

## Stats Cards

Four cards, unchanged structure except for Card 4:

1. **Calls** — unchanged.
2. **Tokens** — unchanged.
3. **Duration** — unchanged.
4. **Status / Unresolved** — **conditional slot**:
   - When `unresolved_count == 0`: renders existing `TurnStatusBadge + FinishBadge` (current behavior).
   - When `unresolved_count > 0`: renders `⚠ Unresolved N` card (amber tinted, `bg-amber-50 border-amber-200`). Subtitle: `possible capture gap`. Clickable — click highlights all amber pointers in the narrative pane (pulse for ~1s) and scrolls to the first one.

**Definition:**

```
unresolved_count =
  |{ tu_id | index.get(tu_id).resolution == null
             AND not (legit-pending condition met) }|
+ |{ tr_id | index.get(tr_id).origin == null }|
```

Legit pending (Case 2) is **not counted** — it's an expected terminal state, not a data-quality signal.

## Relation to Prior Spec

This spec **amends** [2026-04-21-turn-detail-redesign-design.md](./2026-04-21-turn-detail-redesign-design.md) in three sections, and leaves everything else (top bar, Gantt nav, keyboard shortcuts, URL deep-linking, raw-HTTP drawer, loading/error states, phasing) intact.

| Prior spec section | Amendment |
|---|---|
| "User Input Card" and "Final Answer Card" | **Removed.** Content renders inside Call#1 Input and Call#last Output respectively. |
| "Call Card — Expanded" subsections | Restructured. The prior spec lists four subsections (Reasoning / Message / Tool calls / Meta row). This spec replaces the top three with `Input · request body` and `Output · response body` subsections, rendered via the wire-api dispatcher. The meta row and `View raw HTTP →` link remain at the bottom. |
| "Cross-Call Join" backend work (Phase 3) | No longer assumes N+1. If/when that backend work happens, it builds and persists the same turn-scoped index defined in this spec. Until then, the index is built client-side and no backend change ships. |
| Stats card 4 ("Status") | Gains a conditional `⚠ Unresolved` variant. |

## Testing Strategy

- **`buildToolIndex` unit tests** (`console/src/lib/turn-index.test.ts`, new file):
  - Healthy case: tool_use in #1, tool_result in #2 → origin+resolution both set.
  - Parallel tools: three tool_use blocks in #1 all resolving in #2 → three entries, all healthy.
  - Mid-turn gap: tool_use in #2 with no matching tool_result anywhere, and #3 exists → resolution null; state classifier returns "capture gap" (given turn context).
  - Legit pending: tool_use in final call, turn has end_time + end_turn finish_reason → resolution null; state classifier returns "legit pending."
  - Orphan result: tool_result in #4 with `tool_use_id` not found in any call's response body → origin null, resolution set; state classifier returns "orphan."
  - Wire-api mix: same turn's calls with mixed `wire_api` values (hypothetical) — ensure dispatcher handles each.
- **Pointer state classifier** — pure function, unit tested in isolation with the four conditions.
- **Frontend component tests:**
  - `CallCard` expanded with Call#1 → shows user-input card in Input subsection, no standalone UserCard.
  - `CallCard` expanded with Call#last → shows final-answer card in Output subsection, no standalone FinalAnswerCard.
  - `CallCard` expanded with a middle call carrying two tool_results, one orphan → both render with correct back-pointer styling.
  - `CallCard` expanded with a tool_use that has a mid-turn gap → amber `⚠ result not captured` pointer.
- **Visual snapshot:** one end-to-end snapshot against the 5-call fixture used throughout this brainstorm (Read + Grep → cargo check → Read duckdb.rs → Edit → final answer) to lock the new layout.

## Open Questions

None blocking. Deferred:

- **Server-side index** — moving `buildToolIndex` to the backend (caching on turn) is viable once a turn grows large enough (e.g., 100+ calls) that client-side parsing becomes measurable. Not urgent; revisit when first encountered.
- **Jump animation tuning** — the 600ms ring-flash is borrowed from the nav-click; if users report it's too short or too flashy on high-frequency jumps (⚠ cluster), revisit.
- **Stats card click scope** — current rule highlights *all* amber pointers and scrolls to the first. If turns commonly have >5 gaps, a "next anomaly" cycling interaction may be more ergonomic.
