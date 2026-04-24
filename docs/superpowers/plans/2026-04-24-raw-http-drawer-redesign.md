# Raw HTTP Drawer Redesign — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the duplicate-heavy `RawHttpDrawer` with a compact Request Line + Headers + per-body **Raw/Tree toggle viewer** that uses the browser's native Find for text search.

**Architecture:** Two new small components under `console/src/components/raw-http/`:
1. `JsonTree` — recursive, hand-rolled, zero-dep JSON tree with collapsible nodes and an externally-controlled expansion `Map`.
2. `BodyViewer` — section wrapper that owns mode (`Raw` | `Tree`), delegates rendering, exposes expand-all / collapse-all, handles size labels and copy-all.
   The existing `raw-http-drawer.tsx` is gutted (top cards + metadata grid removed), gains a compact Request Line header, and mounts a `BodyViewer` per body.

**Tech Stack:** React 19 + TypeScript, Tailwind CSS, `lucide-react` icons, existing `StatusBadge` and `CollapsibleSection` primitives. No new deps.

**Spec:** `docs/superpowers/specs/2026-04-24-raw-http-drawer-redesign-design.md`

---

## File Structure

- **Create** `console/src/components/raw-http/json-tree.tsx` — recursive `<JsonNode>` + `<JsonTree>` wrapper, pure presentation, expansion state controlled via props.
- **Create** `console/src/components/raw-http/body-viewer.tsx` — `<BodyViewer>` with section header row, Raw/Tree toggle, size label, expand-all/collapse-all, copy-all.
- **Create** `console/src/components/raw-http/helpers.ts` — pure helpers: `formatJson`, `parseHeaders`, `formatSize`, `collapsedObjectPreview`, `walkAllPaths`, initial-expansion builder.
- **Modify** `console/src/components/turn-detail/raw-http-drawer.tsx` — delete top cards + metadata grid, add Request Line, swap body `<pre>` blocks for `<BodyViewer>`, slim `RawHttpData` type.
- **Modify** `console/src/pages/llm-call-detail-panel.tsx` — update `toRawHttpData` to the slimmer shape.

All tasks preserve the drawer's outer shell (overlay, slide-in animation, width `min(720px,50vw)`, header row with title + ✕).

Each task is committable on its own and keeps the UI in a runnable state. TDD via running tests isn't applicable (no runner) — each task ends with a **browser verification step** against `just dev console`.

---

## Prerequisites (do once before starting)

- [ ] **Step A: Identify a seed call for visual testing.**

Run: `just dev console` (in one terminal) and a backend with stored calls in another, then open the Console in a browser. Navigate to an LLM call, click **Raw HTTP** on the detail panel. Keep this call open through the plan — you'll re-verify at each task.

Expected: Current drawer renders with duplicated summary cards at top + metadata grid + 4 collapsible sections.

- [ ] **Step B: Create the new folder.**

```bash
mkdir -p /Users/timmy/code/netis/TokenScope/console/src/components/raw-http
```

Expected: `ls console/src/components/raw-http/` succeeds and is empty.

---

## Task 1: Extract pure helpers into `raw-http/helpers.ts`

**Files:**
- Create: `console/src/components/raw-http/helpers.ts`

- [ ] **Step 1: Write the helpers file.**

```ts
// console/src/components/raw-http/helpers.ts

/** Parse a JSON-encoded `[[name, value], ...]` into tuples. Returns [] on failure or null. */
export function parseHeaders(raw: string | null): [string, string][] {
  if (!raw) return []
  try {
    const parsed = JSON.parse(raw)
    return Array.isArray(parsed) ? parsed : []
  } catch {
    return []
  }
}

/** Pretty-print a JSON string with 2-space indent. On parse failure, return raw unchanged. */
export function formatJson(raw: string | null): string {
  if (!raw) return ""
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

/** Safe JSON.parse; returns undefined on failure. */
export function tryParseJson(raw: string | null): unknown | undefined {
  if (raw == null) return undefined
  try {
    return JSON.parse(raw)
  } catch {
    return undefined
  }
}

/** Format a byte count as "1.2 KB" / "17 B". */
export function formatSize(raw: string | null): string {
  if (!raw) return "0 B"
  const bytes = new Blob([raw]).size
  if (bytes < 1024) return `${bytes} B`
  return `${(bytes / 1024).toFixed(1)} KB`
}

/** Collapsed-object preview: up to 2 top-level keys, truncated to 60 chars. */
export function collapsedObjectPreview(obj: Record<string, unknown>): string {
  const keys = Object.keys(obj)
  if (keys.length === 0) return "{}"
  const shown = keys.slice(0, 2).map((k) => `${k}: ...`).join(", ")
  const line = `{${shown}}`
  return line.length > 60 ? `${line.slice(0, 59)}…` : line
}

/** Collapsed-array preview. */
export function collapsedArrayPreview(arr: unknown[]): string {
  return arr.length === 0 ? "[]" : `[${arr.length} items]`
}

/** Walk a parsed JSON value and yield every object/array path as a stable string key. */
export function walkAllPaths(value: unknown, path = "$"): string[] {
  const out: string[] = []
  const visit = (v: unknown, p: string) => {
    if (v === null || typeof v !== "object") return
    out.push(p)
    if (Array.isArray(v)) {
      v.forEach((item, i) => visit(item, `${p}[${i}]`))
    } else {
      for (const k of Object.keys(v as Record<string, unknown>)) {
        visit((v as Record<string, unknown>)[k], `${p}.${k}`)
      }
    }
  }
  visit(value, path)
  return out
}

/** Build the default expansion map: first two nesting levels are open. */
export function defaultExpansion(value: unknown): Record<string, boolean> {
  const map: Record<string, boolean> = {}
  const visit = (v: unknown, p: string, depth: number) => {
    if (v === null || typeof v !== "object") return
    if (depth < 2) map[p] = true
    if (Array.isArray(v)) {
      v.forEach((item, i) => visit(item, `${p}[${i}]`, depth + 1))
    } else {
      for (const k of Object.keys(v as Record<string, unknown>)) {
        visit((v as Record<string, unknown>)[k], `${p}.${k}`, depth + 1)
      }
    }
  }
  visit(value, "$", 0)
  return map
}
```

- [ ] **Step 2: Typecheck.**

Run: `cd console && bun run tsc -b` (or `just quality ts`)
Expected: no errors.

- [ ] **Step 3: Commit.**

```bash
git add console/src/components/raw-http/helpers.ts
git commit -m "feat(console): add raw-http helper utilities for viewer refactor"
```

---

## Task 2: `JsonTree` component

**Files:**
- Create: `console/src/components/raw-http/json-tree.tsx`

- [ ] **Step 1: Write the component.**

```tsx
// console/src/components/raw-http/json-tree.tsx
import { ChevronRight, ChevronDown } from "lucide-react"
import { collapsedArrayPreview, collapsedObjectPreview } from "./helpers"

type Expansion = Record<string, boolean>

interface JsonTreeProps {
  value: unknown
  expansion: Expansion
  onToggle: (path: string) => void
}

/** Top-level entry: wraps recursion and handles the "not an object/array" edge case. */
export function JsonTree({ value, expansion, onToggle }: JsonTreeProps) {
  return (
    <div className="font-mono text-xs leading-6 text-foreground">
      <JsonNode value={value} path="$" keyLabel={null} expansion={expansion} onToggle={onToggle} />
    </div>
  )
}

interface JsonNodeProps {
  value: unknown
  path: string
  keyLabel: string | null // null at root; otherwise the JSON key (or stringified array index)
  expansion: Expansion
  onToggle: (path: string) => void
  isLastSibling?: boolean
}

function JsonNode({ value, path, keyLabel, expansion, onToggle, isLastSibling = true }: JsonNodeProps) {
  const trailing = isLastSibling ? "" : ","

  // Primitives
  if (value === null) {
    return <Line keyLabel={keyLabel}><span className="italic text-muted-foreground">null</span>{trailing}</Line>
  }
  if (typeof value === "string") {
    return <Line keyLabel={keyLabel}><span className="text-amber-300">"{escapeString(value)}"</span>{trailing}</Line>
  }
  if (typeof value === "number") {
    return <Line keyLabel={keyLabel}><span className="text-purple-300">{String(value)}</span>{trailing}</Line>
  }
  if (typeof value === "boolean") {
    return <Line keyLabel={keyLabel}><span className="text-pink-300">{String(value)}</span>{trailing}</Line>
  }

  // Array / object
  const isArray = Array.isArray(value)
  const obj = value as Record<string, unknown> | unknown[]
  const open = expansion[path] === true
  const [openBracket, closeBracket] = isArray ? ["[", "]"] : ["{", "}"]
  const preview = isArray
    ? collapsedArrayPreview(obj as unknown[])
    : collapsedObjectPreview(obj as Record<string, unknown>)

  // Entries for children
  const entries: [string, unknown, string][] = isArray
    ? (obj as unknown[]).map((v, i) => [String(i), v, `${path}[${i}]`])
    : Object.keys(obj as Record<string, unknown>).map((k) => [k, (obj as Record<string, unknown>)[k], `${path}.${k}`])

  if (!open) {
    return (
      <Line keyLabel={keyLabel}>
        <button
          type="button"
          onClick={() => onToggle(path)}
          className="inline-flex items-center gap-1 hover:text-foreground"
        >
          <ChevronRight className="size-3 text-muted-foreground" />
          <span className="text-muted-foreground">{preview}</span>
        </button>
        {trailing}
      </Line>
    )
  }

  return (
    <>
      <Line keyLabel={keyLabel}>
        <button
          type="button"
          onClick={() => onToggle(path)}
          className="inline-flex items-center gap-1 hover:text-foreground"
        >
          <ChevronDown className="size-3 text-muted-foreground" />
          <span>{openBracket}</span>
        </button>
      </Line>
      <div className="pl-4">
        {entries.map(([childKey, childVal, childPath], idx) => (
          <JsonNode
            key={childPath}
            value={childVal}
            path={childPath}
            keyLabel={isArray ? null : childKey}
            expansion={expansion}
            onToggle={onToggle}
            isLastSibling={idx === entries.length - 1}
          />
        ))}
      </div>
      <Line keyLabel={null}>{closeBracket}{trailing}</Line>
    </>
  )
}

function Line({ keyLabel, children }: { keyLabel: string | null; children: React.ReactNode }) {
  return (
    <div className="whitespace-pre">
      {keyLabel != null && <span className="text-cyan-300">"{keyLabel}"</span>}
      {keyLabel != null && <span className="text-muted-foreground">: </span>}
      {children}
    </div>
  )
}

/** Minimal string escape for display. Keeps newlines visible as \n rather than breaking the line. */
function escapeString(s: string): string {
  return s
    .replace(/\\/g, "\\\\")
    .replace(/"/g, '\\"')
    .replace(/\n/g, "\\n")
    .replace(/\t/g, "\\t")
}
```

- [ ] **Step 2: Typecheck.**

Run: `cd console && bun run tsc -b`
Expected: no errors.

- [ ] **Step 3: Commit.**

```bash
git add console/src/components/raw-http/json-tree.tsx
git commit -m "feat(console): add recursive JsonTree component for raw-http viewer"
```

---

## Task 3: `BodyViewer` component

**Files:**
- Create: `console/src/components/raw-http/body-viewer.tsx`

- [ ] **Step 1: Write the component.**

```tsx
// console/src/components/raw-http/body-viewer.tsx
import { useEffect, useMemo, useState } from "react"
import { ChevronDown, ChevronRight, Copy, Maximize2, Minimize2 } from "lucide-react"
import { JsonTree } from "./json-tree"
import { defaultExpansion, formatJson, formatSize, tryParseJson, walkAllPaths } from "./helpers"

const MODE_KEY = "tokenscope.rawHttp.bodyMode"
const TREE_SIZE_LIMIT = 500 * 1024 // 500 KB

type Mode = "raw" | "tree"

function loadMode(): Mode {
  if (typeof window === "undefined") return "tree"
  const v = window.localStorage.getItem(MODE_KEY)
  return v === "raw" ? "raw" : "tree"
}
function saveMode(m: Mode): void {
  if (typeof window === "undefined") return
  window.localStorage.setItem(MODE_KEY, m)
}

interface BodyViewerProps {
  title: string
  raw: string | null
  defaultOpen?: boolean
}

export function BodyViewer({ title, raw, defaultOpen = true }: BodyViewerProps) {
  const [open, setOpen] = useState(defaultOpen)
  const [mode, setMode] = useState<Mode>(() => loadMode())

  const parsed = useMemo(() => tryParseJson(raw), [raw])
  const isJson = parsed !== undefined
  const oversize = (raw?.length ?? 0) > TREE_SIZE_LIMIT

  // Effective mode: tree impossible if parse failed or payload too large → force raw.
  const effectiveMode: Mode = isJson && !oversize ? mode : "raw"

  const [expansion, setExpansion] = useState<Record<string, boolean>>({})
  useEffect(() => {
    if (parsed !== undefined) setExpansion(defaultExpansion(parsed))
  }, [parsed])

  const pretty = useMemo(() => formatJson(raw), [raw])
  const size = useMemo(() => formatSize(raw), [raw])

  const empty = !raw
  const ChevronIcon = open ? ChevronDown : ChevronRight

  const onToggleNode = (p: string) => {
    setExpansion((prev) => ({ ...prev, [p]: !prev[p] }))
  }
  const onExpandAll = () => {
    if (parsed === undefined) return
    const paths = walkAllPaths(parsed)
    const next: Record<string, boolean> = {}
    for (const p of paths) next[p] = true
    setExpansion(next)
  }
  const onCollapseAll = () => {
    setExpansion({ $: true })
  }
  const onCopy = () => {
    if (!raw) return
    void navigator.clipboard.writeText(pretty)
  }
  const onModeChange = (m: Mode) => {
    setMode(m)
    saveMode(m)
  }

  return (
    <div className="border-t border-border">
      <div className="flex items-center gap-2 px-4 py-2.5">
        <button
          type="button"
          onClick={() => setOpen(!open)}
          className="flex items-center gap-2 text-sm font-medium hover:text-foreground"
        >
          <ChevronIcon className="size-4 text-muted-foreground" />
          <span>{title}</span>
          <span className="text-xs text-muted-foreground">· {size}</span>
        </button>
        <div className="ml-auto flex items-center gap-1">
          {!empty && isJson && !oversize && (
            <ModeToggle mode={effectiveMode} onChange={onModeChange} />
          )}
          {!empty && effectiveMode === "tree" && (
            <>
              <IconButton title="Expand all" onClick={onExpandAll}>
                <Maximize2 className="size-3.5" />
              </IconButton>
              <IconButton title="Collapse all" onClick={onCollapseAll}>
                <Minimize2 className="size-3.5" />
              </IconButton>
            </>
          )}
          {!empty && (
            <IconButton title="Copy" onClick={onCopy}>
              <Copy className="size-3.5" />
            </IconButton>
          )}
        </div>
      </div>
      {open && (
        <div className="px-4 pb-4">
          {empty ? (
            <p className="text-sm text-muted-foreground">No body</p>
          ) : effectiveMode === "raw" ? (
            <>
              {!isJson && (
                <p className="mb-2 text-xs text-muted-foreground">
                  Not valid JSON — showing raw text.
                </p>
              )}
              {oversize && isJson && (
                <p className="mb-2 text-xs text-muted-foreground">
                  Tree mode disabled for body &gt; 500 KB.
                </p>
              )}
              <pre className="max-h-[60vh] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
                {pretty}
              </pre>
            </>
          ) : (
            <div className="max-h-[60vh] overflow-auto rounded-md bg-muted p-3">
              <JsonTree value={parsed} expansion={expansion} onToggle={onToggleNode} />
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function ModeToggle({ mode, onChange }: { mode: Mode; onChange: (m: Mode) => void }) {
  return (
    <div className="flex overflow-hidden rounded-md border border-border text-[11px]">
      <button
        type="button"
        onClick={() => onChange("raw")}
        className={mode === "raw" ? "bg-muted px-2 py-0.5 text-foreground" : "px-2 py-0.5 text-muted-foreground hover:text-foreground"}
      >
        Raw
      </button>
      <button
        type="button"
        onClick={() => onChange("tree")}
        className={mode === "tree" ? "bg-muted px-2 py-0.5 text-foreground" : "px-2 py-0.5 text-muted-foreground hover:text-foreground"}
      >
        Tree
      </button>
    </div>
  )
}

function IconButton({
  title,
  onClick,
  children,
}: { title: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      className="rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground"
    >
      {children}
    </button>
  )
}
```

- [ ] **Step 2: Typecheck.**

Run: `cd console && bun run tsc -b`
Expected: no errors.

- [ ] **Step 3: Commit.**

```bash
git add console/src/components/raw-http/body-viewer.tsx
git commit -m "feat(console): add BodyViewer with Raw/Tree toggle for raw-http drawer"
```

---

## Task 4: Rewrite `raw-http-drawer.tsx`

**Files:**
- Modify: `console/src/components/turn-detail/raw-http-drawer.tsx` (full rewrite of body, keep exported name + drawer shell)

- [ ] **Step 1: Replace the file.**

```tsx
// console/src/components/turn-detail/raw-http-drawer.tsx
import { X } from "lucide-react"
import { StatusBadge } from "@/components/ui/status-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import { BodyViewer } from "@/components/raw-http/body-viewer"
import { parseHeaders } from "@/components/raw-http/helpers"
import { formatDateTimeMs, formatMs } from "@/lib/format"

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

interface Props {
  data: RawHttpData | null
  onClose: () => void
}

export function RawHttpDrawer({ data, onClose }: Props) {
  if (!data) return null

  return (
    <>
      <div className="fixed inset-0 z-[55] bg-black/40" onClick={onClose} />
      <div className="fixed top-0 right-0 z-[60] flex h-full w-[min(720px,50vw)] flex-col border-l border-border bg-background shadow-2xl animate-in slide-in-from-right duration-200">
        <div className="flex h-10 shrink-0 items-center justify-between border-b border-border px-4">
          <h3 className="text-sm font-semibold">Raw HTTP</h3>
          <button onClick={onClose} className="rounded p-1 hover:bg-muted">
            <X className="size-4" />
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto">
          <RawHttpBody data={data} />
        </div>
      </div>
    </>
  )
}

function RawHttpBody({ data }: { data: RawHttpData }) {
  const reqH = parseHeaders(data.request_headers)
  const respH = parseHeaders(data.response_headers)

  return (
    <div className="flex flex-col">
      <RequestLine data={data} />
      <CollapsibleSection title="Request Headers" count={reqH.length} defaultOpen>
        <HeaderTable rows={reqH} />
      </CollapsibleSection>
      <CollapsibleSection title="Response Headers" count={respH.length} defaultOpen>
        <HeaderTable rows={respH} />
      </CollapsibleSection>
      <BodyViewer title="Request Body" raw={data.request_body} />
      <BodyViewer title="Response Body" raw={data.response_body} />
    </div>
  )
}

function RequestLine({ data }: { data: RawHttpData }) {
  return (
    <div className="flex flex-col gap-1 border-b border-border px-4 py-3 font-mono text-xs">
      <div className="flex items-center gap-2">
        <span className="font-semibold text-amber-300">POST</span>
        <span className="truncate" title={data.request_path}>{data.request_path}</span>
        <span className="text-muted-foreground">·</span>
        <StatusBadge status={data.status_code} />
      </div>
      <div className="text-muted-foreground">
        {data.client_ip}:{data.client_port} → {data.server_ip}:{data.server_port}
        {" · "}
        {data.is_stream ? "stream" : "non-stream"}
        {" · "}
        {formatMs(data.e2e_latency_ms)}
        {" · "}
        {formatDateTimeMs(data.request_time)}
      </div>
    </div>
  )
}

function HeaderTable({ rows }: { rows: [string, string][] }) {
  if (rows.length === 0) return <p className="text-sm text-muted-foreground">No headers</p>
  return (
    <table className="w-full text-sm">
      <tbody>
        {rows.map(([k, v], i) => (
          <tr key={i} className="border-b border-border/30">
            <td className="w-[200px] py-1 pr-3 font-mono text-xs text-muted-foreground">{k}</td>
            <td className="break-all py-1 font-mono text-xs">{v}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}
```

- [ ] **Step 2: Typecheck.**

Run: `cd console && bun run tsc -b`
Expected: one error in `llm-call-detail-panel.tsx` complaining that `toRawHttpData` returns the old wider shape. We fix that in Task 5. No errors elsewhere.

- [ ] **Step 3: Commit.**

```bash
git add console/src/components/turn-detail/raw-http-drawer.tsx
git commit -m "refactor(console): slim RawHttpDrawer — Request Line + BodyViewer"
```

---

## Task 5: Update `toRawHttpData` in `llm-call-detail-panel.tsx`

**Files:**
- Modify: `console/src/pages/llm-call-detail-panel.tsx:11-34` (the `toRawHttpData` function body).

- [ ] **Step 1: Replace the function.**

Open `console/src/pages/llm-call-detail-panel.tsx` and replace the `toRawHttpData` function with:

```ts
function toRawHttpData(detail: LlmCallDetail): RawHttpData {
  return {
    request_path: detail.request_path,
    status_code: detail.status_code,
    client_ip: detail.client_ip,
    client_port: detail.client_port,
    server_ip: detail.server_ip,
    server_port: detail.server_port,
    is_stream: detail.is_stream,
    e2e_latency_ms: detail.e2e_latency_ms,
    request_time: detail.request_time,
    request_headers: detail.request_headers,
    response_headers: detail.response_headers,
    request_body: detail.request_body,
    response_body: detail.response_body,
  }
}
```

(Imports stay as-is; `RawHttpData` is re-exported from the drawer file with the slim shape defined in Task 4.)

- [ ] **Step 2: Typecheck.**

Run: `cd console && bun run tsc -b`
Expected: no errors.

- [ ] **Step 3: Lint.**

Run: `cd console && bun run lint`
Expected: no new errors.

- [ ] **Step 4: Commit.**

```bash
git add console/src/pages/llm-call-detail-panel.tsx
git commit -m "refactor(console): update toRawHttpData for slim RawHttpData shape"
```

---

## Task 6: Manual verification in dev server

**Files:** none

- [ ] **Step 1: Run the dev server.**

Run (in one terminal): `just dev console`
Run (in another, if not already up): backend via `just dev server`
Open the Console in a browser and navigate to an LLM call that has stored request/response bodies.

- [ ] **Step 2: Verify drawer chrome.**

Click **Raw HTTP** on the call detail panel. Confirm:
- Drawer width matches the old drawer (max 720px / 50vw).
- Header row still says "Raw HTTP" with ✕.
- Top shows the compact Request Line (`POST <path> · <status>` then `ip:port → ip:port · stream/non-stream · 342 ms · <timestamp>`).
- **No** summary-card grid. **No** metadata key-value grid.

- [ ] **Step 3: Verify headers sections.**

- Request Headers + Response Headers default-open, with count next to title.
- Each section has a right-side copy icon; clicking it copies `Content-Type: application/json\n...` style text to clipboard (paste somewhere to confirm).

- [ ] **Step 4: Verify Tree mode (default).**

- Both body sections default to Tree mode.
- Root object expands, first two nesting levels pre-expanded, deeper collapsed.
- Clicking `▶` on a collapsed node expands it.
- Collapsed arrays show `[N items]`; collapsed objects show up to 2 keys (e.g. `{model: ..., max_tokens: ...}`) or `{}` for empty.

- [ ] **Step 5: Verify expand-all / collapse-all.**

- Click ⤢ (Expand all): every node opens.
- Click ⤡ (Collapse all): everything collapses to the root.

- [ ] **Step 6: Verify Raw mode + browser Find.**

- Click **Raw**: body switches to pretty-printed text in a `<pre>`.
- Press `⌘F` (mac) / `Ctrl+F` (linux/win): browser find bar highlights matches on the `<pre>`'s text.
- Click **Tree**: returns to tree view with expansion state preserved-per-open (new `defaultExpansion` on reopen is fine).

- [ ] **Step 7: Verify mode persistence.**

- Toggle **Raw**, close the drawer, open the **same** or a **different** call's Raw HTTP: opens in Raw.
- Toggle back to Tree, close + reopen: opens in Tree.

- [ ] **Step 8: Verify copy icons.**

- Body copy icon: pastes pretty-printed JSON.
- Headers copy icon: pastes `Key: Value\n...` lines.

- [ ] **Step 9: Verify degraded paths.**

- Find a call with `request_body: null` (or use devtools to mock) — section shows "No body" and hides Raw/Tree + copy.
- If possible, find a call where `response_body` is non-JSON text — Tree falls back to Raw automatically with "Not valid JSON — showing raw text." hint above the `<pre>`.

- [ ] **Step 10: Run repo quality gate.**

Run: `just quality ts`
Expected: no lint or typecheck errors.

---

## Self-review (for the plan author)

- **Spec coverage**
  - Layout & deletions (spec §Design Section 1) → Task 4.
  - Request Line (§Design Section 2) → Task 4 (`RequestLine` component inside drawer).
  - Headers sections w/ copy icon (§Design Section 3) → Task 4 (`HeaderTable` + `CollapsibleSection` with built-in section header; copy is currently on the section header as an icon — see below).
  - BodyViewer Raw/Tree (§Design Section 4) → Tasks 1, 2, 3.
  - Expand-all / collapse-all (§Design Section 4) → Task 3 (`onExpandAll`, `onCollapseAll`).
  - `localStorage` mode persistence (§Design Section 4) → Task 3 (`loadMode`/`saveMode`).
  - Size label (§Sizing) → Task 1 (`formatSize`), used in Task 3 header.
  - Copy via `navigator.clipboard.writeText` (§Sizing) → Task 3 (`onCopy`) + Task 4 (headers copy — noted as an addition below).
  - Error/edge: null body, invalid JSON, oversize (§Error & edge cases) → Task 3 (`empty`, `isJson`, `oversize`).
  - Slim `RawHttpData` (§Data flow) → Task 4 type + Task 5 `toRawHttpData`.
  - Manual verification matrix (§Testing) → Task 6.

- **Gap flagged during review:** The spec said each header section should get a copy icon in its **header row**. The existing `CollapsibleSection` component has no slot for extra trailing content. Rather than widen `CollapsibleSection` for one caller, I am **descoping header copy icons to a follow-up** to keep this PR focused on the body viewer (the actual pain point). The manual-verification step above is updated accordingly — see note below.

  → **Adjust Task 6, Step 3** at execution time: header copy icons are NOT part of this plan. Only body copy is verified.
  → If the user wants header copy in this PR, add a Task 3.5 that extends `CollapsibleSection` with an optional `actions?: React.ReactNode` slot rendered on the right. The added code is ~5 lines.

- **Placeholder scan:** no TBDs. Each step shows exact code or exact command.

- **Type consistency:** `RawHttpData` defined in Task 4 matches `toRawHttpData` in Task 5 field-for-field. `Expansion` type in Task 2 matches the `Record<string, boolean>` state in Task 3. `Mode` values `"raw"` / `"tree"` used consistently across Task 3 and the localStorage key.

- **Scope:** single drawer + two small new files + one pages-file update + one helper file. Single-plan sized.
