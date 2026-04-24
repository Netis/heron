# Raw HTTP Drawer Redesign

**Date:** 2026-04-24
**Status:** Design
**Owner:** frontend (`console/`)

## Problem

The existing "View Raw HTTP" drawer (`console/src/components/turn-detail/raw-http-drawer.tsx`) opens on top of the LLM Call Detail panel and suffers from two concrete problems:

1. **Top section is redundant.** The 4 summary cards (Wire API / Status / TTFT / Tokens) are byte-for-byte the same as `SummaryCards` on the parent panel. The metadata grid (ID / Path / Client / Server / Stream / Req Time) duplicates 5 of 6 rows with the parent's `MetadataGrid`.
2. **Body is unnavigable.** `request_body` and `response_body` are rendered as a single pretty-printed `<pre>`, capped at `max-h-[400px]` with scroll. For a 3–20 KB JSON payload (typical for multi-turn chat requests or tool-rich Anthropic responses) finding a specific field is a visual scan through hundreds of lines of text.

User requirement: *"Use Chrome DevTools as the design reference. The critical capability is searching body text."*

## Goals

- Eliminate everything on the drawer that duplicates the parent panel.
- Give each body its own **structural navigation** (collapsible tree) and a **text-search** fallback.
- Add no new npm dependencies — hand-roll the JSON tree, rely on the browser's native Find for text search.

## Non-goals

- Tabbed layout (Chrome DevTools' `Headers / Payload / Preview / Response / Timing`). A single-scroll layout is retained; each concern gets its own collapsible section in the existing flow.
- Rendering the literal SSE wire event log for streaming calls. Streaming responses are already reconstructed into a single JSON object upstream (`server/ts-llm/src/wire_apis/anthropic.rs:290` `build_response_body`; same pattern in `openai/chat.rs:225`). Changing that is a separate backend concern.
- JSON syntax highlighting in Raw mode. Tree mode already colors keys and value types; Raw stays plain to keep the diff small.

## Design

### Layout (drawer top → bottom)

Drawer shell unchanged: fixed right-side panel, width `min(720px, 50vw)`, full height, existing slide-in animation.

```
┌────────────────────────────────────────────────┐
│ Raw HTTP                                     ✕ │  ← unchanged header
├────────────────────────────────────────────────┤
│ POST /v1/messages · 200 OK                     │  ← new: compact Request Line
│ 10.0.1.12:55324 → 10.0.2.8:443 ·               │     (2 lines, mono font)
│ stream · 342 ms · 2026-04-24 10:12:08.341      │
├────────────────────────────────────────────────┤
│ ▼ Request Headers (14)                    ⧉    │  ← existing, + copy icon
│   content-type: application/json               │
│   …                                            │
├────────────────────────────────────────────────┤
│ ▼ Response Headers (8)                    ⧉    │  ← existing, + copy icon
│   …                                            │
├────────────────────────────────────────────────┤
│ ▼ Request Body · 3.2 KB   [Raw|Tree] ⇱⇲ ⧉     │  ← new body viewer
│   ▼ {                                          │
│     "model": "claude-sonnet-4",                │
│     "max_tokens": 4096,                        │
│     ▶ "messages": [3 items]                    │
│   }                                            │
├────────────────────────────────────────────────┤
│ ▼ Response Body · 18.4 KB [Raw|Tree] ⇱⇲ ⧉     │
│   …                                            │
└────────────────────────────────────────────────┘
```

### Section 1 — Deletions

Remove from `raw-http-drawer.tsx`:

- The 4-column summary grid (lines ~80–101 of the current file).
- The 6-row metadata key-value grid (lines ~102–116).
- `RawHttpData` shrinks accordingly: drop `wire_api`, `model`, `status_code`, `finish_reason`, `ttft_ms`, `e2e_latency_ms`, `input_tokens`, `output_tokens`, `is_stream`, and keep only what the Request Line and Headers/Body sections need.

### Section 2 — Request Line

A single new block under the drawer header, 2 lines of `font-mono text-xs`:

- Line 1: `POST <path> · <status>` — method hardcoded to `POST` (all wire APIs we support are POST; `LlmCallDetail` carries no method field and we don't add one); `path` from `detail.request_path`; status uses the existing `StatusBadge` component.
- Line 2 (`text-muted-foreground`): `<client_ip>:<client_port> → <server_ip>:<server_port> · <stream|non-stream> · <e2e_ms> ms · <request_time formatted with ms>`.

Purpose: once the drawer covers the parent, the user needs a 1-glance answer to "which request am I looking at?" without any duplicated card grid.

### Section 3 — Header sections

No structural change. The existing `CollapsibleSection` with count in title, and the 2-column table below, stay as-is. Add a small `⧉ copy` icon button in the section header that copies the headers as `Key: Value\n...` text to the clipboard.

### Section 4 — Body sections (new component)

Replace the current `CollapsibleSection` + `<pre>{formatJson(raw)}</pre>` with a new `<BodyViewer>` component.

**BodyViewer props:**
```ts
interface BodyViewerProps {
  title: string                // "Request Body" | "Response Body"
  raw: string | null           // the serialized JSON string (may be non-JSON if backend ever stores raw bytes)
  defaultOpen?: boolean        // default true
}
```

**Section header row (left → right):**
- `▼/▶` toggle + title + ` · <size-in-KB>` label.
- Right side: `[Raw | Tree]` mode toggle, `⇱` expand-all, `⇲` collapse-all (Tree mode only; hidden in Raw mode), `⧉` copy-all.

**Mode state:**
- Per-viewer (each body has its own mode), default `Tree`.
- Persisted to `localStorage` under a single key (`tokenscope.rawHttp.bodyMode`) so the user's preference sticks across opens.

**Raw mode:**
```tsx
<pre className="max-h-[60vh] overflow-auto font-mono text-xs">
  {prettyPrintedJson}
</pre>
```
`prettyPrintedJson = JSON.stringify(JSON.parse(raw), null, 2)` with a try/catch fallback to the original `raw` string if parsing fails (current `formatJson` in `raw-http-drawer.tsx` already implements this — reuse the helper).

The browser's native Find (`⌘F` / `Ctrl+F`) searches and highlights against DOM text with no custom widget.

**Tree mode:**

Hand-rolled recursive component, no new deps. Single file `console/src/components/raw-http/json-tree.tsx`.

- Parse `raw` with `JSON.parse`; on parse failure, render a small warning and fall back to the Raw view.
- Root `<JsonNode>` renders one of: primitive (string / number / boolean / null), array, object.
- Arrays/objects render a collapsible header (`▼/▶` + key + preview) followed by indented children when expanded.
- Collapsed preview:
  - Objects: `{key1: ..., key2: ...}` showing up to 2 top-level keys, full line truncated to **60 chars** with `…`; empty object → `{}`.
  - Arrays: `[N items]` where `N = array.length`; empty array → `[]`.
- Primitive colors (Tailwind classes, from existing palette):
  - string → `text-amber-300` (matches existing yellow-ish literal coloring elsewhere if present, else a close Tailwind token)
  - number → `text-purple-300`
  - boolean → `text-pink-300`
  - null → `text-muted-foreground italic`
  - keys → `text-cyan-300`
- Expansion state:
  - Keyed by node path (e.g. `$.messages[0].content`).
  - Initial state: first two levels auto-expanded; deeper levels collapsed.
  - Controlled by a `Map<string, boolean>` held in `<BodyViewer>` state.
- **Expand-all / Collapse-all** buttons in the section header rewrite the expansion map:
  - Expand-all: walks the entire parsed value, sets every path → `true`.
  - Collapse-all: clears the map (or sets depth-0 → `true`, everything else false, to keep the root visible).

**No copy-path-on-hover.** Explicitly dropped per user preference — keeps the hover state uncluttered.

**Performance:** For bodies larger than some threshold (say 500 KB), skip the Tree mode default and start in Raw mode with a small notice (`"Tree mode disabled for body > 500 KB"`). Tree recursion over multi-MB JSONs blows up React reconciliation on expand-all. If we never see such payloads in practice, this threshold is cheap insurance.

### Data flow

`LlmCallDetailPanel` already constructs `RawHttpData` via `toRawHttpData(detail)`. The slimmer shape means:

```ts
// NEW
export interface RawHttpData {
  request_path: string
  status_code: number | null
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  is_stream: boolean
  e2e_latency_ms: number | null
  request_time: number
  request_headers: string | null
  response_headers: string | null
  request_body: string | null
  response_body: string | null
}
```

### Sizing & copy behavior

- Size label (`· 3.2 KB`) computed from `new Blob([raw]).size / 1024`, formatted to one decimal. Null bodies display `· 0 B`.
- Copy icons use `navigator.clipboard.writeText` and flash a 1s "Copied" tooltip.

### Error & edge cases

- `request_body` or `response_body` is `null` → render "No body" muted line inside the section; Raw/Tree toggle and copy icon hidden.
- `JSON.parse` fails on body content → Tree mode falls back to Raw automatically and shows a small muted hint `"Not valid JSON — showing raw text"`.
- `request_headers` / `response_headers` already parsed through `parseHeaders` (tolerates non-JSON → `[]`); keep current behavior.

### Testing

The frontend has no test runner today (`console/package.json` defines no test script). Setting one up for a single visual-component change is disproportionate — verify manually via the existing dev server (`just dev console`) against a real backend with stored calls.

**Manual test matrix** (run each after the implementation is wired up):

- Non-stream Anthropic response (~2 KB) — Tree expands correctly, types colored, collapsed previews match the rule.
- Stream Anthropic response reconstructed JSON (~10 KB) — first-two-level auto-expand stays readable; expand-all handles it without visible lag; collapse-all returns to a small view.
- OpenAI Responses with reasoning blocks (~15 KB) — deeply nested arrays render; `[N items]` preview counts match.
- Empty body (`""` or `null`) — shows "No body", Raw/Tree toggle and copy icon are hidden.
- Non-JSON body (paste bogus text via devtools if no real case) — Tree falls through to Raw with "Not valid JSON — showing raw text" hint.
- `localStorage` persistence — toggle Raw, close drawer, reopen another call: opens in Raw.
- `⌘F` / `Ctrl+F` in Raw mode — browser find highlights matches against the `<pre>` content.

## Files touched

- `console/src/components/turn-detail/raw-http-drawer.tsx` — strip top cards & metadata grid, swap in Request Line + new BodyViewer for each body.
- `console/src/components/raw-http/json-tree.tsx` — new, the recursive tree renderer.
- `console/src/components/raw-http/body-viewer.tsx` — new, the Raw/Tree toggle container (lives in the same new folder for locality).
- `console/src/pages/llm-call-detail-panel.tsx` — update `toRawHttpData` to the slimmer shape.

## Open questions (to resolve during implementation)

1. **Colors.** Match the existing dark theme palette used in `call-renderers/*`. If those files already define JSON-ish token colors, reuse them; else pick the nearest Tailwind tokens and keep the set small (key / string / number / boolean / null).

## Out of scope / follow-ups

- Actual SSE event stream viewer (would need backend to preserve raw chunks; speculative).
- JSON search inside Tree mode (e.g. filter to matches). User decided Ctrl+F in Raw mode covers this need.
- Syntax highlighting in Raw mode.
