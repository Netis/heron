# Agent Sessions UI — Design

## Goal

Expose the new backend session endpoints (`/api/agent-sessions*`) in the React console. Sessions are the container above turns; the UI should read as "here is the user's work session" rather than as another ops dashboard of dimensions.

Scope: a session list page and a session detail page, plus the small backend tweak required to make the detail page's transcript view practical.

Non-goals: no new metrics, no new backend entities, no changes to the top-level Agent Turns / LLM Calls pages.

## Guiding principles

- **Session is a user-story concept, not a dimension.** Global toolbar filters that describe LLM traffic (`wire_api`, `model`, `server_ip`) don't apply here. `agent_kind` is a badge, not a primary column.
- **Session detail is for reading the conversation.** The transcript is the substance; stats are context. Each turn's user message and final answer render inline.
- **Reuse what exists.** The existing `<Markdown>` component, `AgentTurnDetailPanel` slide-over, filter dropdowns, and row styling conventions stay unchanged.

## Routes & navigation

### Routes

| Path | Page |
| --- | --- |
| `/agent-sessions` | Session list |
| `/agent-sessions/:source_id/:session_id` | Session detail (transcript) |

Both routes live under the existing `AppLayout`.

### Sidebar

Add a new "Agent Sessions" nav item **above** "Agent Turns" (session contains turns). Use a conversation-shaped icon from `lucide-react` (e.g. `MessagesSquare`-like, distinct from the Agent Turns icon — exact icon chosen at implementation time; the two must be visually distinguishable at the 16px sidebar size).

### Toolbar behavior on session pages

The global toolbar keeps time preset / start / end / refresh (time window is passed to the session list endpoint as `start` / `end`). The `wire_api` / `model` / `server_ip` chips are **hidden** when `location.pathname` starts with `/agent-sessions` — they don't apply and showing them grayed-out would be noise. Other pages are unaffected.

### Page-local filter strip

Below the toolbar, `/agent-sessions` (list page) carries a thin filter strip with two dropdowns:

- **Source** — populated from `/api/filters` (same values the other pages use).
- **Agent kind** — multi-select, options like `claude-cli`, `codex-cli`.

The detail page has no filter strip (single session).

## Session list page

### Layout

```
┌─ global toolbar (wire_api/model/server_ip hidden) ─────────┐
├─ page filter strip (Source ▾ · Agent kind ▾) ──────────────┤
├─ session rows (inbox style, one row per session) ──────────┤
│  [claude-cli] a1b2c3d4…                             16:30  │
│  fix the bug in auth middleware where …             2h28m  │
│  4 turns · 23 calls · 82k tok · $1.23                      │
│ ────────────────────────────────────────────────────────── │
│  …                                                          │
└─ [Load more] ──────────────────────────────────────────────┘
```

### Row content

Each row has three horizontal regions:

- **Left column (flex: 1)**: agent-kind badge + truncated `session_id` (monospace, muted); primary line = `first_user_input_preview` (ellipsis, one line); stats line = `{turn_count} turns · {call_count} calls · {tokens_total} tok · {cost}` (muted).
- **Right column (fixed width)**: relative "last activity" timestamp (e.g. `16:30`, `yesterday`, `3d ago`, from `last_turn_at_in_window`) and total duration (`first_turn_at → last_turn_at`).
- Whole row is clickable — navigates to the detail page, preserving the current toolbar query params.

Cost shows as `$1.23` when `total_cost_usd` is non-null; otherwise omitted.

### Data source

- `GET /api/agent-sessions?start&end&source_id&agent_kind&cursor&page_size` (already implemented).
- Cursor pagination via TanStack Query's `useInfiniteQuery`. "Load more" calls `fetchNextPage()`. Disabled / spinner while `isFetchingNextPage`.
- No total count (the endpoint doesn't return one; cursor pagination doesn't need it).

### Empty / error / loading

Match the existing Agent Turns page exactly:

- Initial load: centered `Loader2` spinner.
- Error: inline `text-destructive` row with `error.message`.
- Empty: muted "No sessions found in the selected time range".

## Session detail page

### Layout

```
← Agent Sessions                                       (back link)
┌─ session header strip ────────────────────────────────────┐
│ [claude-cli] a1b2c3d4-…  source: default                   │
│          4 turns · 23 calls · 82k tok · $1.23 · 2h 28m    │
└───────────────────────────────────────────────────────────┘

 14:02  👤 USER      fix the bug in auth middleware …          ← collapsed
        🎯 ASSISTANT Found the issue in auth/middleware.ts …
        ▾  ● ok · 46s · 3 calls · 16.1k in / 1.9k out

 14:40  ┌─ EXPANDED ─────────────────────────────────────────┐
        │ 👤 USER · 14:40:12.417                              │
        │ now also cover the case where refresh returns 5xx… │
        │                                                    │
        │ 🎯 ASSISTANT                                        │
        │ Added a try/catch around the refresh call with …   │
        │ (rendered with the existing <Markdown> component)  │
        │                                                    │
        │ ▴ ● ok · 1m 12s · 5 calls · 22.3k in / 1.7k out    │
        │                          View turn detail →        │
        └────────────────────────────────────────────────────┘

                         [Load older turns]
```

### Header strip

Rendered by `SessionHeader` (pure presentational). Content:

- Agent-kind badge (same colour convention as the list row).
- `session_id` (full, monospace, selectable).
- `source: <source_id>` label.
- Right-aligned stats line: `N turns · N calls · N tok · $X.XX · H m` (duration = `last_turn_at - first_turn_at`).

Uses the existing `cn` utility and Tailwind tokens — no new design primitives.

### Transcript rendering

Each turn is a `TurnBlock` component that renders in one of two modes based on a single local `expanded` boolean.

**Collapsed mode** (default):

- One line for USER: `👤 USER` label + single-line preview of `user_input`, truncated by the frontend to 120 chars with ellipsis. Uses the existing blue (`border-blue-200` / `bg-blue-50/60`) accent.
- One line for ASSISTANT: `🎯 ASSISTANT` label + single-line preview of `final_answer`, truncated to 120 chars. Uses the existing emerald accent. If `final_answer` is null / empty (turn ended without a final answer), render "Turn ended without a final answer" in italic muted text on a red-tinted background.
- Metadata strip below: `▾ chevron ● status-dot · duration · call_count calls · tokens_in in / tokens_out out`. The status dot uses the existing `TurnStatusBadge` colour palette. The strip is the click target.

**Expanded mode**:

- Same cards, but rendered at full size with the existing `<Markdown>` component on the full `user_input` / `final_answer` text (not the frontend-truncated preview). USER card includes the precise timestamp `HH:MM:SS.mmm`.
- The metadata strip becomes `▴ chevron ● status · …                                     View turn detail →`.
- Multiple turns can be expanded simultaneously — no accordion behavior.

### Interactions

- **Toggle expand**: clicking the metadata strip (the only click target on a turn). User/assistant text stays selectable because their cards don't intercept clicks.
- **View turn detail**: link in the expanded strip opens the existing `AgentTurnDetailPanel` (slide-over), reusing it unchanged. The panel's internal URL state (`?call=N&raw=1`) continues to work — this page's route becomes the slide-over's background route.
- **Back navigation**: `← Agent Sessions` link in the top-left navigates to `/agent-sessions` preserving toolbar params. Browser back also works.

### State

- `expandedTurns: Set<string>` — local component state in `agent-session-detail.tsx`. Not URL-persisted.
- `selectedTurnId: string | null` — controls the `AgentTurnDetailPanel` slide-over. Also local component state, matching the existing pattern on the Agent Turns page.

### Data fetching

- Session header → `GET /api/agent-sessions/:source_id/:session_id` (already implemented).
- Transcript rows → `GET /api/agent-sessions/:source_id/:session_id/turns?cursor&page_size` (backend switching to cursor pagination, see Backend API changes). Each row is a `SessionTurnItem` carrying full `user_input` and `final_answer` (server-side reconstructed from referenced call bodies; details below). Page size default 50, clamped to 1–200. `useInfiniteQuery`; "Load older turns" calls `fetchNextPage()`.
- Expand toggle is pure client state — no network round-trip.

## Backend API changes

Scoped to the session-turns endpoint; everything else stays.

### `ts-storage` (`server/ts-storage/src/query.rs`)

Replace `SessionTurnsQuery`'s `page` / `page_size` with a cursor:

```rust
pub struct SessionTurnsCursor {
    pub start_time_us: i64,
    pub turn_id: String,  // tiebreaker when two turns share a start_time
}

pub struct SessionTurnsQuery {
    pub source_id: String,
    pub session_id: String,
    pub cursor: Option<SessionTurnsCursor>,
    pub page_size: u32,
}
```

Add `encode_session_turns_cursor` / `decode_session_turns_cursor`, hex-JSON format matching the existing `encode_session_cursor` / `decode_session_cursor`.

Add two new response types:

```rust
pub struct SessionTurnItem {
    // Identical to AgentTurnListItem except:
    pub user_input: Option<String>,      // full text (not preview)
    pub final_answer: Option<String>,    // full text (not preview)
    // … all other AgentTurnListItem fields (turn_id, session_id, start_time,
    //   end_time, duration_ms, wire_api, agent_kind, primary_model, models_used,
    //   call_count, total_input_tokens, total_output_tokens, status,
    //   final_finish_reason)
}

pub struct SessionTurnsPage {
    pub items: Vec<SessionTurnItem>,
    pub next_cursor: Option<String>,
}
```

The old `TurnsPage` type continues to be used by `/api/agent-turns` (unchanged).

### `ts-storage` DuckDB implementation (`server/ts-storage/src/duckdb.rs`)

Rewrite `query_session_turns`.

**Paging query** (the easy part):

- SELECT all `AgentTurnListItem` columns plus `user_input_preview`, `user_call_id`, `final_answer_preview`, `final_call_id` from `agent_turns`.
- `WHERE source_id = ? AND session_id = ?` plus, if cursor is present: `AND (start_time, turn_id) < (?, ?)`.
- `ORDER BY start_time DESC, turn_id DESC`.
- `LIMIT page_size + 1` using the existing fetch-one-extra pattern to compute `next_cursor` without a COUNT.

**Full-text extraction** (the work):

Full `user_input` / `final_answer` are **not stored on `agent_turns`** — only previews are. The column `user_input_preview` is capped at 500 chars with a trailing `…` when truncated, and full text is reconstructed on demand by loading the referenced call body (`llm_calls.request_body` for user, `.response_body` for assistant) and running the agent profile's extractor. This is the pattern `query_turn_by_id` already uses via the `extract_full_text` helper.

For a single turn-detail request that pattern is fine. For a session-turns page (up to 200 turns), per-turn lookups would mean up to 400 extra round-trips. Instead, we batch:

1. Walk the paged turns, build two lists:
   - For each turn where `user_input_preview` ends with `…` (was truncated), push `user_call_id` into a `needs_extract_user` list; otherwise the preview **is** the full text (already guaranteed by `truncate_preview`) and we use it directly.
   - Same for `final_answer_preview` / `final_call_id` into `needs_extract_assistant`.
2. Batch-fetch the relevant call bodies in **one** query each:
   - `SELECT id, wire_api, request_body FROM llm_calls WHERE id IN (?, ?, …)` for the user-input extractions.
   - `SELECT id, wire_api, response_body FROM llm_calls WHERE id IN (?, ?, …)` for the assistant-text extractions.
   These are PK `IN` lookups — no JOIN, respecting `CLAUDE.md`'s read-path rule.
3. Run the profile extractor (`profile.extract_user_input` / `profile.extract_assistant_text`) on each fetched body. Reuse the per-turn `agent_kind` to select the profile, same as `extract_full_text` does today.
4. Stitch results back onto the `SessionTurnItem`s keyed by call id. Turns whose previews were already full text skip all of this.

If a call body is missing or the extractor declines, fall back to the preview string (same contract as `extract_full_text`).

Refactor note: lift the single-call extraction helper into a batch-capable variant. Keep `extract_full_text` (used by `query_turn_by_id`) as a thin wrapper over the batch API, or leave it in place and add a new `extract_full_text_batch` next to it — whichever keeps the diff small. Implementation chooses at coding time.

### Cost envelope

- Short messages (preview not truncated): 1 query total (the paging query). This is common — user messages are often one line.
- Long messages: 1 paging query + up to 2 batched IN queries (one for user bodies, one for assistant bodies). Bounded regardless of page size.
- Profile extraction parses the call body in memory. Request bodies for Anthropic include full conversation history (can be large — low MB range for deep sessions). The extractor reads only the last user message, so the parse work is proportional to message count in the body, not token count.

### `ts-api` route (`server/ts-api/src/routes/agent_sessions.rs`)

`SessionTurnsParams`: drop `page`, add `cursor: Option<String>`. Handler decodes cursor, calls `storage.query_session_turns`, returns the `SessionTurnsPage`.

### Storage backend trait + other backends

`StorageBackend::query_session_turns`'s return type changes from `Result<TurnsPage>` to `Result<SessionTurnsPage>`. The no-op sink implementation (`server/ts-storage/src/sink.rs`) returns `SessionTurnsPage { items: vec![], next_cursor: None }`. No other backend currently implements the trait.

### Tests

Update the DuckDB test `query_session_by_id_and_turns_roundtrip` to exercise cursor-based pagination (current test uses page/page_size).

## Frontend component breakdown

### New files

```
console/src/
├── pages/
│   ├── agent-sessions.tsx              ← list page
│   └── agent-session-detail.tsx        ← detail page (routed)
├── components/
│   └── session-detail/
│       ├── index.ts
│       ├── session-header.tsx
│       ├── turn-block.tsx
│       └── turn-metadata-strip.tsx
└── hooks/
    └── use-agent-sessions.ts           ← useAgentSessions (list, infinite),
                                          useAgentSessionDetail,
                                          useSessionTurns (infinite, cursor)
```

### Touched files

- `console/src/app.tsx` — register `/agent-sessions` and `/agent-sessions/:source_id/:session_id`.
- `console/src/components/layout/sidebar.tsx` — add "Agent Sessions" nav item above "Agent Turns".
- `console/src/components/layout/toolbar.tsx` — hide `wire_api` / `model` / `server_ip` chips when on `/agent-sessions*`.
- `console/src/types/api.ts` — add `SessionListItem`, `SessionsPage`, `SessionDetail`, `SessionTurnItem`, `SessionTurnsPage`.

### Component contracts

- **`agent-sessions.tsx`** (list): owns page filter strip state (`source_id`, `agent_kind`), drives `useAgentSessions(...)`. Renders inbox rows inline (kept as local components — no need to factor out `SessionRow` unless it grows).
- **`agent-session-detail.tsx`** (detail): reads `source_id` / `session_id` from route params. Owns `expandedTurns` and `selectedTurnId` state. Composes `<SessionHeader>` + list of `<TurnBlock>` + load-more button + optional `<AgentTurnDetailPanel>`.
- **`SessionHeader`**: `(detail: SessionDetail) => JSX`. Pure presentational.
- **`TurnBlock`**: `(props: { turn: SessionTurnItem; expanded: boolean; onToggle: () => void; onInspect: (turnId: string) => void }) => JSX`. Handles its own preview truncation (`user_input.slice(0, 120) + "…"` when overflow) and markdown rendering when expanded.
- **`TurnMetadataStrip`**: the click-target row underneath each turn. Props: `turn`, `expanded`, `onToggle`, optional `onInspect`.

### Hooks

```ts
// useInfiniteQuery — cursor-based
useAgentSessions({ sourceId, agentKind })    // → pages: SessionsPage[]
useAgentSessionDetail(sourceId, sessionId)   // → SessionDetail | undefined
useSessionTurns(sourceId, sessionId)         // → pages: SessionTurnsPage[]
```

Session list pulls `start` / `end` from `useToolbarStore` (same pattern as `useAgentTurns`). Session detail + turns do not — they're keyed by IDs only.

## Error handling, edge cases

- **Session not found** (detail route with bad ID): `GET /api/agent-sessions/:source/:id` returns 404; detail page shows a "Session not found" inline message with a "Back to sessions" button.
- **Session with zero turns in window** (shouldn't happen given session existence implies turns exist, but defensively): detail page's transcript section shows "No turns in this session" muted.
- **Incomplete turns** (no `final_answer`): render the placeholder described in Transcript rendering. The turn still expands normally — expanded view shows the full `user_input` and the placeholder.
- **Long session** (hundreds of turns): cursor pagination keeps each page bounded. If a user hits "Load older turns" many times, each page is ≤ 200 turns and payload stays reasonable.
- **Slow render with many expanded turns**: if a user expands dozens of verbose assistant answers, `<Markdown>` rendering could become costly. If this proves a problem in practice we can memoize per-turn rendering or virtualize the transcript — out of scope for v1.

## Out of scope

- Searching / filtering inside a session (no text search across turns).
- Exporting a session transcript.
- Linking LLM Calls or HTTP Exchanges pages to a session.
- Any changes to the overview / performance / traffic / errors / models pages.
- Any changes to the sink.rs storage backend beyond a no-op implementation update.
