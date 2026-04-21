# Turn Detail Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Redesign the Agent Turn detail panel around an agent-behavior narrative (User → call timeline with tool calls/reasoning/text → Final Answer), and extend the backend to surface parsed call content.

**Architecture:** Frontend rewrite of `AgentTurnDetailPanel` into focused sub-components with a vertical-Gantt sidebar serving as both time-overview and navigation. Backend extends the existing `WireApi` trait with two new methods (`parse_output` / `parse_input`) and pipes parsed content through `/api/agent-turns/{id}/calls` and `/api/llm-calls/{id}`. Ships in three independently-shippable phases.

**Tech Stack:** Rust (Tokio, Axum, serde_json, duckdb), React 19 + TypeScript, Tailwind, TanStack Query, react-router v7, lucide-react, Bun + Vite.

**Commit discipline:** Per the user's directive, **one commit per phase** (not per task). Within a phase, tasks accumulate changes; the final task of each phase is "commit the phase".

**Spec:** `docs/superpowers/specs/2026-04-21-turn-detail-redesign-design.md` is the authoritative source. This plan operationalizes it.

**Tooling notes:**
- Backend tests use `cargo test -p <crate> <test_name>` (crate names: `ts-llm`, `ts-turn`, `ts-storage`, `ts-api`).
- Frontend has **no test runner** configured. Frontend validation = `cd console && bun run build` (typecheck + Vite build) + `bun run lint` + manual dev-server verification via `just dev console`. Frontend tasks use acceptance checks, not unit tests.
- `just quality rs` and `just quality ts` are the repo-wide gates. Run them at the end of each phase before committing.

---

## File Structure

### Phase 1 — Frontend (new + modified files)

**Split today's 676-line `agent-turn-detail-panel.tsx` into focused components:**

- Modify: `console/src/pages/agent-turn-detail-panel.tsx` — becomes a thin orchestrator: data fetching, overall panel frame, URL sync, state.
- Create: `console/src/components/turn-detail/top-bar.tsx` — title, meta summary on the right, `ⓘ` popover, `✕`.
- Create: `console/src/components/turn-detail/stats-cards.tsx` — 4 compact summary cards.
- Create: `console/src/components/turn-detail/gantt-nav.tsx` — vertical Gantt sidebar (navigation + time overview).
- Create: `console/src/components/turn-detail/user-card.tsx` — `👤 User` card with long-content collapse.
- Create: `console/src/components/turn-detail/final-answer-card.tsx` — `🎯 Final Answer` card.
- Create: `console/src/components/turn-detail/call-card.tsx` — collapsed + expanded call card.
- Create: `console/src/components/turn-detail/raw-http-drawer.tsx` — right-side secondary drawer for HTTP details.
- Create: `console/src/components/turn-detail/metadata-popover.tsx` — cold-metadata popover triggered by `ⓘ`.
- Create: `console/src/hooks/use-turn-url-state.ts` — read/write `?call=N&raw=1` URL params.

### Phase 2 — Backend parsers + list enrichment

- Modify: `server/ts-llm/src/model.rs` — add `ParsedOutput`, `ParsedInput`, `ParsedToolCall`, `ParsedToolResult`, `ToolResultKind` types; extend `WireApi` trait with `parse_output` / `parse_input` methods (default returns empty).
- Modify: `server/ts-llm/src/wire_apis/anthropic.rs` — implement `parse_output` + `parse_input`.
- Modify: `server/ts-llm/src/wire_apis/openai.rs` — implement `parse_output` + `parse_input` for both `OpenAiChatWireApi` and `OpenAiResponsesWireApi`.
- Create: `server/ts-llm/tests/fixtures/anthropic_output_tool_use.json`, `anthropic_input_tool_result.json`, etc. (6 files × 3 wire APIs = 18 fixtures)
- Create: `scripts/dump-call-bodies.sh` — one-time helper to pull bodies from a running DuckDB to seed fixtures.
- Modify: `server/ts-storage/src/query.rs` — add `request_body: Option<String>` and `response_body: Option<String>` to `TurnCallItem`.
- Modify: `server/ts-storage/src/duckdb.rs` — extend `query_turn_calls` SQL to select bodies.
- Modify: `server/ts-storage/src/sink.rs` — update test double.
- Create: `server/ts-api/src/routes/turn_call_enrichment.rs` — pure functions that take a `TurnCallItem` + `WireApi` and produce the enriched API payload.
- Modify: `server/ts-api/src/routes/agent_turns.rs` — wire the enrichment into the `calls` handler.
- Modify: `console/src/types/api.ts` — extend `AgentTurnCallItem` with `type`, `tool_calls[]`, `has_reasoning`, `reasoning_preview`, `message_preview`.
- Modify: `console/src/components/turn-detail/call-card.tsx` — render tool-name chips + previews + type icons.
- Modify: `console/src/components/turn-detail/gantt-nav.tsx` — use real `type` for icons.
- Modify: `console/src/components/turn-detail/stats-cards.tsx` — render the Calls type-breakdown line.

### Phase 3 — Full parsed detail + tool-result join

- Create: `server/ts-turn/src/tool_result_join.rs` — `attach_tool_results` function.
- Modify: `server/ts-storage/src/query.rs` — add `parsed: Option<ParsedCallContent>` to `CallDetail` (or a sibling type).
- Modify: `server/ts-storage/src/duckdb.rs` — extend `query_call_by_id` to also fetch the successor call's body (same turn, next sequence).
- Modify: `server/ts-api/src/routes/llm_calls.rs` — enrich `detail` handler with parsed + joined result.
- Modify: `console/src/types/api.ts` — extend `LlmCallDetail` with `parsed`.
- Modify: `console/src/components/turn-detail/call-card.tsx` — render Reasoning / Message / Tool-calls subsections with per-tool result.

---

## Phase 1 — Frontend Refactor

No backend changes. Ships immediately-usable layout even if phases 2/3 are deferred.

### Task 1: Scaffold component directory + stub files

**Files:**
- Create: `console/src/components/turn-detail/index.ts`
- Create: all `console/src/components/turn-detail/*.tsx` listed in File Structure § Phase 1 (as empty-export stubs)
- Create: `console/src/hooks/use-turn-url-state.ts` (stub)

- [ ] **Step 1.1: Create directory with barrel + empty component stubs**

Write `console/src/components/turn-detail/index.ts`:

```ts
export { TopBar } from "./top-bar"
export { StatsCards } from "./stats-cards"
export { GanttNav } from "./gantt-nav"
export { UserCard } from "./user-card"
export { FinalAnswerCard } from "./final-answer-card"
export { CallCard } from "./call-card"
export { RawHttpDrawer } from "./raw-http-drawer"
export { MetadataPopover } from "./metadata-popover"
```

Each stub file (e.g. `top-bar.tsx`) should export a named component returning `null`:

```tsx
export function TopBar() {
  return null
}
```

This gives the rest of the plan concrete import targets to edit.

- [ ] **Step 1.2: Create `use-turn-url-state.ts` stub**

```ts
import { useSearchParams } from "react-router"

export function useTurnUrlState() {
  const [params, setParams] = useSearchParams()
  const call = params.get("call") ? Number(params.get("call")) : null
  const raw = params.get("raw") === "1"
  return { call, raw, setParams }
}
```

- [ ] **Step 1.3: Verify build passes**

Run: `cd console && bun run build`
Expected: PASS (tsc + Vite exit 0)

---

### Task 2: Top Bar component + metadata popover

**Files:**
- Modify: `console/src/components/turn-detail/top-bar.tsx`
- Modify: `console/src/components/turn-detail/metadata-popover.tsx`
- Modify: `console/src/pages/agent-turn-detail-panel.tsx` — replace existing title row with `<TopBar>`.

- [ ] **Step 2.1: Implement `MetadataPopover`**

```tsx
import { X } from "lucide-react"
import { formatDateTimeMs } from "@/lib/format"
import type { AgentTurnDetail } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  onClose: () => void
}

export function MetadataPopover({ turn, onClose }: Props) {
  const rows: [string, string][] = [
    ["Turn ID", turn.turn_id],
    ["Session ID", turn.session_id],
    ["Agent", turn.agent_kind],
    ["Tenant", turn.tenant_id ?? "—"],
    ["Wire API", turn.wire_api],
    ["Start", formatDateTimeMs(turn.start_time)],
    ["End", formatDateTimeMs(turn.end_time)],
    ["Models", turn.models_used.join(", ") || "—"],
    ["Subagents", turn.subagents_used.join(", ") || "—"],
  ]
  return (
    <div className="absolute right-10 top-10 z-10 w-[420px] rounded-lg border border-border bg-background p-4 shadow-xl">
      <div className="mb-3 flex items-center justify-between">
        <h3 className="text-sm font-semibold">Metadata</h3>
        <button onClick={onClose} className="rounded p-1 hover:bg-muted">
          <X className="size-4" />
        </button>
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
        {rows.map(([k, v]) => (
          <div key={k} className="contents">
            <span className="text-muted-foreground">{k}</span>
            <span className="break-all font-mono text-xs" title={v}>{v}</span>
          </div>
        ))}
      </div>
    </div>
  )
}
```

- [ ] **Step 2.2: Implement `TopBar`**

```tsx
import { useState } from "react"
import { Info, X } from "lucide-react"
import { MetadataPopover } from "./metadata-popover"
import type { AgentTurnDetail } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  onClose: () => void
}

function truncateMid(s: string, head = 8, tail = 6): string {
  return s.length > head + tail + 1 ? `${s.slice(0, head)}…${s.slice(-tail)}` : s
}

export function TopBar({ turn, onClose }: Props) {
  const [metaOpen, setMetaOpen] = useState(false)
  return (
    <div className="relative flex h-10 shrink-0 items-center justify-between border-b border-border px-4">
      <h2 className="text-sm font-semibold">Agent Turn Detail</h2>
      <div className="flex items-center gap-3 text-xs text-muted-foreground">
        <span>{turn.agent_kind}</span>
        <span>·</span>
        <span>{turn.tenant_id ?? "—"}</span>
        <span>·</span>
        <span className="font-mono" title={turn.turn_id}>{truncateMid(turn.turn_id)}</span>
        <button
          onClick={() => setMetaOpen((o) => !o)}
          className="rounded p-1 hover:bg-muted hover:text-foreground"
          aria-label="Show metadata"
        >
          <Info className="size-4" />
        </button>
        <button onClick={onClose} className="rounded p-1 hover:bg-muted hover:text-foreground">
          <X className="size-4" />
        </button>
      </div>
      {metaOpen && <MetadataPopover turn={turn} onClose={() => setMetaOpen(false)} />}
    </div>
  )
}
```

- [ ] **Step 2.3: Wire `<TopBar>` into the panel**

In `agent-turn-detail-panel.tsx`, locate the existing `<div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">` header (~line 643). Replace the entire header div with `<TopBar turn={turn} onClose={onClose} />`. Keep the outer panel container and loading/error branches unchanged for now.

- [ ] **Step 2.4: Build + dev-server check**

Run: `cd console && bun run build`
Expected: PASS.

Manual: `just dev console`, open a turn detail. Verify the new top bar renders with the agent/tenant/turn-id summary, the `ⓘ` button opens the popover, `✕` closes the panel.

---

### Task 3: Stats cards

**Files:**
- Modify: `console/src/components/turn-detail/stats-cards.tsx`
- Modify: `console/src/pages/agent-turn-detail-panel.tsx` — use `<StatsCards>` in place of the current `SummaryCard` grid.

- [ ] **Step 3.1: Implement `StatsCards`**

```tsx
import { useMemo } from "react"
import { formatDuration, formatMs, formatNumber } from "@/lib/format"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"
import { cn } from "@/lib/utils"

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  onJumpToSlowest?: (sequence: number) => void
}

function Card({ label, children, className }: {
  label: string
  children: React.ReactNode
  className?: string
}) {
  return (
    <div className={cn("flex flex-col gap-0.5 rounded-lg border border-border bg-muted/30 px-3 py-2", className)}>
      <span className="text-xs text-muted-foreground">{label}</span>
      {children}
    </div>
  )
}

export function StatsCards({ turn, calls, onJumpToSlowest }: Props) {
  const slowest = useMemo(() => {
    let best: AgentTurnCallItem | null = null
    for (const c of calls) {
      if (c.e2e_latency_ms == null) continue
      if (!best || (c.e2e_latency_ms > (best.e2e_latency_ms ?? 0))) best = c
    }
    return best
  }, [calls])

  return (
    <div className="grid grid-cols-4 gap-3">
      <Card label="Calls">
        <div className="text-sm font-medium tabular-nums">{turn.call_count}</div>
        {/* Phase 2 will add: type breakdown row here */}
      </Card>
      <Card label="Tokens">
        <div className="flex items-center gap-3 text-sm font-medium tabular-nums">
          <span className="flex flex-col"><span className="text-[10px] text-muted-foreground">in</span><span>{formatNumber(turn.total_input_tokens)}</span></span>
          <span className="flex flex-col"><span className="text-[10px] text-muted-foreground">out</span><span>{formatNumber(turn.total_output_tokens)}</span></span>
        </div>
        {turn.total_cost_usd != null && (
          <div className="text-xs text-muted-foreground tabular-nums">${turn.total_cost_usd.toFixed(2)}</div>
        )}
      </Card>
      <Card label="Duration">
        <div className="text-sm font-medium tabular-nums">{formatDuration(turn.duration_ms)}</div>
        {slowest && (
          <button
            onClick={() => onJumpToSlowest?.(slowest!.sequence)}
            className="text-left text-xs text-muted-foreground hover:text-foreground tabular-nums"
          >
            slowest #{slowest.sequence} {formatMs(slowest.e2e_latency_ms)}
          </button>
        )}
      </Card>
      <Card label="Status / Finish">
        <div className="flex items-center gap-2">
          <TurnStatusBadge status={turn.status} />
          <FinishBadge reason={turn.final_finish_reason} />
        </div>
      </Card>
    </div>
  )
}
```

- [ ] **Step 3.2: Wire into panel**

In `agent-turn-detail-panel.tsx`, replace the 4-card `<SummaryCard>` grid inside the existing `TurnDetailView` with `<StatsCards turn={turn} calls={calls} onJumpToSlowest={seq => { /* wired in Task 4 */ }} />`. Remove the `CollapsibleSection` "Metadata" block (popover replaces it).

- [ ] **Step 3.3: Build + dev-server check**

Run: `cd console && bun run build`.
Manual: open a turn with cost populated → verify `$X.XX` appears under Tokens; verify slowest-call chip appears and is clickable.

---

### Task 4: Vertical Gantt nav sidebar

**Files:**
- Modify: `console/src/components/turn-detail/gantt-nav.tsx`
- Modify: `console/src/pages/agent-turn-detail-panel.tsx` — replace left sidebar with `<GanttNav>`.

- [ ] **Step 4.1: Implement `GanttNav`**

```tsx
import { useMemo } from "react"
import { Wrench, MessageSquare, Target, CircleDot } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatDuration, formatMs } from "@/lib/format"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  activeSequence: number | null
  onSelect: (sequence: number) => void
}

const SLOW_THRESHOLD_MS = 10_000

function classifySpeed(call: AgentTurnCallItem): "normal" | "slow" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}

// Phase 1 has no parsed `type`; fall back to neutral icon.
function TypeIcon({ call }: { call: AgentTurnCallItem }) {
  // Type-aware variant switches in Phase 2 (see spec § Left Nav).
  void call
  return <CircleDot className="size-3 text-muted-foreground" />
}

export function GanttNav({ turn, calls, activeSequence, onSelect }: Props) {
  const { minStart, total } = useMemo(() => {
    if (calls.length === 0) return { minStart: turn.start_time, total: turn.duration_ms || 1 }
    const min = Math.min(...calls.map((c) => c.request_time))
    const max = Math.max(...calls.map((c) => c.complete_time ?? c.response_time ?? c.request_time))
    return { minStart: min, total: Math.max(max - min, 1) }
  }, [calls, turn])

  return (
    <aside className="flex w-[140px] shrink-0 flex-col border-r border-border">
      <div className="shrink-0 border-b border-border px-3 py-2">
        <div className="text-xs font-medium">Timeline</div>
        <div className="text-[11px] tabular-nums text-muted-foreground">{formatDuration(turn.duration_ms)}</div>
      </div>
      <div className="flex-1 overflow-y-auto p-1">
        {calls.length === 0 ? (
          <div className="flex h-20 items-center justify-center text-xs text-muted-foreground">No calls</div>
        ) : (
          calls.map((c) => {
            const end = c.complete_time ?? c.response_time ?? c.request_time
            const offset = ((c.request_time - minStart) / total) * 100
            const width = Math.max(((end - c.request_time) / total) * 100, 0.5)
            const speed = classifySpeed(c)
            return (
              <button
                key={c.id}
                onClick={() => onSelect(c.sequence)}
                className={cn(
                  "grid w-full grid-cols-[16px_16px_1fr_36px] items-center gap-1 rounded px-1 py-1 text-left text-[10px]",
                  activeSequence === c.sequence ? "bg-blue-50 dark:bg-blue-950/40" : "hover:bg-muted/60",
                  speed === "slow" && "border-l-2 border-amber-500/70",
                  speed === "error" && "border-l-2 border-red-500/70",
                )}
              >
                <span className="tabular-nums text-muted-foreground">{c.sequence}</span>
                <TypeIcon call={c} />
                <div className="relative h-2 rounded bg-muted">
                  <div
                    className={cn(
                      "absolute top-0 h-full rounded",
                      speed === "slow" && "bg-amber-500/80",
                      speed === "error" && "bg-red-500/80",
                      speed === "normal" && "bg-blue-400",
                    )}
                    style={{ left: `${offset}%`, width: `${width}%`, minWidth: "2px" }}
                  />
                </div>
                <span className={cn(
                  "text-right tabular-nums",
                  speed === "slow" && "text-amber-600",
                  speed === "error" && "text-red-600",
                  speed === "normal" && "text-muted-foreground",
                )}>
                  {formatMs(c.e2e_latency_ms)}
                </span>
              </button>
            )
          })
        )}
      </div>
    </aside>
  )
}
```

- [ ] **Step 4.2: Wire into panel**

Replace the existing `<aside>` block (~line 597 of `agent-turn-detail-panel.tsx`) with `<GanttNav turn={turn} calls={calls} activeSequence={activeSeq} onSelect={handleSelect} />`.

Local state in the panel:

```tsx
const [activeSeq, setActiveSeq] = useState<number | null>(null)

const handleSelect = (seq: number) => {
  setActiveSeq(seq)
  const el = document.getElementById(`call-${seq}`)
  el?.scrollIntoView({ behavior: "smooth", block: "start" })
}
```

Pass `handleSelect` to `StatsCards` as `onJumpToSlowest` too.

- [ ] **Step 4.3: Build + dev-server check**

Manual: open a turn. Verify bars render on a shared time axis (gaps visible), clicking a row scrolls the main pane (even though there's no scrollable content yet — next task adds it). Slow calls (>10s) show amber border + amber bar. Error calls show red.

---

### Task 5: User Input + Final Answer cards

**Files:**
- Modify: `console/src/components/turn-detail/user-card.tsx`
- Modify: `console/src/components/turn-detail/final-answer-card.tsx`
- Modify: `console/src/pages/agent-turn-detail-panel.tsx` — drop the 3-tab structure; render `<UserCard>` + call list + `<FinalAnswerCard>` inline.

- [ ] **Step 5.1: Implement `UserCard`**

```tsx
import { useState } from "react"
import { Markdown } from "@/components/ui/markdown"
import { formatDateTimeMs } from "@/lib/format"

interface Props {
  text: string
  startTime: number
}

export function UserCard({ text, startTime }: Props) {
  const [expanded, setExpanded] = useState(false)
  const long = text.split("\n").length > 8 || text.length > 600
  return (
    <div className="rounded-lg border border-blue-200 bg-blue-50/60 p-4 dark:border-blue-900 dark:bg-blue-950/30">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-sm font-medium">👤 User</span>
        <span className="text-xs tabular-nums text-muted-foreground">{formatDateTimeMs(startTime)}</span>
      </div>
      <div className={long && !expanded ? "max-h-[240px] overflow-hidden" : ""}>
        <Markdown text={text} />
      </div>
      {long && (
        <button
          onClick={() => setExpanded((e) => !e)}
          className="mt-2 text-xs text-muted-foreground hover:text-foreground"
        >
          {expanded ? "Show less ▴" : "Show more ▾"}
        </button>
      )}
    </div>
  )
}
```

- [ ] **Step 5.2: Implement `FinalAnswerCard`**

```tsx
import { Markdown } from "@/components/ui/markdown"
import { formatMs } from "@/lib/format"
import type { AgentTurnCallItem } from "@/types/api"

interface Props {
  text: string
  finalCall?: AgentTurnCallItem
  onJumpToCall?: (sequence: number) => void
}

export function FinalAnswerCard({ text, finalCall, onJumpToCall }: Props) {
  return (
    <div className="rounded-lg border border-emerald-200 bg-emerald-50/60 p-4 dark:border-emerald-900 dark:bg-emerald-950/30">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-sm font-medium">🎯 Final Answer</span>
        {finalCall && (
          <button
            onClick={() => onJumpToCall?.(finalCall.sequence)}
            className="text-xs tabular-nums text-muted-foreground hover:text-foreground"
          >
            #{finalCall.sequence} · {formatMs(finalCall.e2e_latency_ms)}
          </button>
        )}
      </div>
      <Markdown text={text} />
    </div>
  )
}
```

- [ ] **Step 5.3: Remove tab structure in the panel**

In `agent-turn-detail-panel.tsx`, gut the existing `TurnDetailView` function. Replace its body with:

```tsx
function TurnDetailView({
  turn,
  calls,
  activeSeq,
  onSelect,
}: {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  activeSeq: number | null
  onSelect: (seq: number) => void
}) {
  const finalCall = calls.find((c) => c.id === turn.final_call_id) ?? calls[calls.length - 1]

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="shrink-0 p-4 pb-0">
        <StatsCards turn={turn} calls={calls} onJumpToSlowest={onSelect} />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        <div className="flex flex-col gap-3">
          {turn.user_input && <UserCard text={turn.user_input} startTime={turn.start_time} />}
          {calls.map((c) => (
            <CallCard key={c.id} call={c} active={c.sequence === activeSeq} />
          ))}
          {turn.final_answer
            ? <FinalAnswerCard text={turn.final_answer} finalCall={finalCall} onJumpToCall={onSelect} />
            : calls.length > 0 && (
                <p className="text-center text-xs text-muted-foreground">Turn ended without a final answer</p>
              )}
        </div>
      </div>
    </div>
  )
}
```

Delete the old `TabButton`, `EmptyTabContent`, `TurnGantt` definitions — the Gantt lives in the sidebar now, the tab structure is gone. (The `TurnGantt` code can be retained temporarily if useful, but delete before commit.)

- [ ] **Step 5.4: Build check**

Run: `cd console && bun run build`.

The `CallCard` component is implemented in Task 6; until then, reference its current stub export. Build must still pass because the stub exists.

---

### Task 6: Call card (collapsed state, Phase 1 — no parsed data yet)

**Files:**
- Modify: `console/src/components/turn-detail/call-card.tsx`

- [ ] **Step 6.1: Implement collapsed call card**

```tsx
import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { FinishBadge } from "@/components/ui/finish-badge"
import type { AgentTurnCallItem } from "@/types/api"

const SLOW_THRESHOLD_MS = 10_000

function classify(call: AgentTurnCallItem): "normal" | "slow" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}

interface Props {
  call: AgentTurnCallItem
  active?: boolean
  defaultExpanded?: boolean
  onOpenRawHttp?: (id: string) => void
}

export function CallCard({ call, active, defaultExpanded, onOpenRawHttp }: Props) {
  const [expanded, setExpanded] = useState(Boolean(defaultExpanded))
  const speed = classify(call)
  return (
    <div
      id={`call-${call.sequence}`}
      className={cn(
        "rounded-lg border bg-background transition-colors",
        speed === "slow" && "border-l-2 border-l-amber-500/70 border-border",
        speed === "error" && "border-l-2 border-l-red-500/70 border-border",
        speed === "normal" && "border-border",
        active && "ring-2 ring-blue-400 ring-offset-1",
      )}
    >
      <button
        onClick={() => setExpanded((e) => !e)}
        className="flex w-full items-center gap-3 px-3 py-2 text-left"
      >
        <span className="w-6 shrink-0 tabular-nums text-xs text-muted-foreground">#{call.sequence}</span>
        {/* Phase 2 renders real type icons + tool chips here. */}
        <FinishBadge reason={call.finish_reason} />
        <span className="flex-1 truncate text-xs text-muted-foreground">{call.model}</span>
        <span className={cn(
          "shrink-0 text-xs tabular-nums",
          speed === "slow" && "text-amber-600",
          speed === "error" && "text-red-600",
          speed === "normal" && "text-muted-foreground",
        )}>
          {speed === "error" && "✗ "}{formatMs(call.e2e_latency_ms)}
        </span>
        <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
          {formatNumber(call.input_tokens)}↑ {formatNumber(call.output_tokens)}↓
        </span>
        {expanded ? <ChevronDown className="size-4 text-muted-foreground" /> : <ChevronRight className="size-4 text-muted-foreground" />}
      </button>
      {expanded && (
        <div className="border-t border-border px-3 py-2 text-xs text-muted-foreground">
          <div>{call.wire_api} · TTFB {formatMs(call.ttfb_ms)} · finish: {call.finish_reason ?? "—"}</div>
          <button
            onClick={() => onOpenRawHttp?.(call.id)}
            className="mt-2 text-foreground hover:underline"
          >
            View raw HTTP →
          </button>
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 6.2: Build + dev-server check**

Manual: open a turn. Scrolling through calls shows cards; clicking a card expands inline showing meta row + "View raw HTTP →" link. The link is a no-op until Task 7 wires it.

---

### Task 7: Raw HTTP drawer

**Files:**
- Modify: `console/src/components/turn-detail/raw-http-drawer.tsx`
- Modify: `console/src/pages/agent-turn-detail-panel.tsx` — mount drawer + pass `onOpenRawHttp` to `CallCard`.

- [ ] **Step 7.1: Implement `RawHttpDrawer`**

Reuse logic from the existing `LlmCallDetailView` (in the same file as the panel): the 4-card stats + `CallTimelineBar` + metadata rows + 4 `CollapsibleSection` blocks for headers/bodies. Extract into the new component. Drawer shell:

```tsx
import { X, Loader2 } from "lucide-react"
import { useLlmCallDetail } from "@/hooks/use-llm-call-detail"
import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import { formatDateTimeMs, formatMs, formatNumber } from "@/lib/format"
import { cn } from "@/lib/utils"

interface Props {
  callId: string | null
  onClose: () => void
}

function parseHeaders(raw: string | null): [string, string][] {
  if (!raw) return []
  try { return JSON.parse(raw) } catch { return [] }
}

function formatJson(raw: string | null): string {
  if (!raw) return ""
  try { return JSON.stringify(JSON.parse(raw), null, 2) } catch { return raw }
}

export function RawHttpDrawer({ callId, onClose }: Props) {
  const { data: detail, isLoading, isError } = useLlmCallDetail(callId)
  if (!callId) return null

  return (
    <div className="fixed top-0 right-0 z-[60] flex h-full w-[min(720px,50vw)] flex-col border-l border-border bg-background shadow-2xl animate-in slide-in-from-right duration-200">
      <div className="flex h-10 shrink-0 items-center justify-between border-b border-border px-4">
        <h3 className="text-sm font-semibold">Raw HTTP</h3>
        <button onClick={onClose} className="rounded p-1 hover:bg-muted">
          <X className="size-4" />
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        {isLoading && !detail ? (
          <div className="flex h-40 items-center justify-center"><Loader2 className="size-5 animate-spin text-muted-foreground" /></div>
        ) : isError || !detail ? (
          <p className="text-sm text-destructive">Failed to load HTTP details</p>
        ) : (
          <RawHttpBody detail={detail} />
        )}
      </div>
    </div>
  )
}

function RawHttpBody({ detail }: { detail: NonNullable<ReturnType<typeof useLlmCallDetail>["data"]> }) {
  const reqH = parseHeaders(detail.request_headers)
  const respH = parseHeaders(detail.response_headers)
  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 gap-3">
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">Wire API / Model</div>
          <div>{detail.wire_api}</div>
          <div className="text-muted-foreground">{detail.model}</div>
        </div>
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">Status / Finish</div>
          <div className="flex items-center gap-2">
            <StatusBadge status={detail.status_code} />
            <FinishBadge reason={detail.finish_reason} />
          </div>
        </div>
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">TTFB / E2E</div>
          <div className="tabular-nums">{formatMs(detail.ttfb_ms)} / {formatMs(detail.e2e_latency_ms)}</div>
        </div>
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">Tokens</div>
          <div className="tabular-nums">{formatNumber(detail.input_tokens)}↑ / {formatNumber(detail.output_tokens)}↓</div>
        </div>
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
        {[
          ["ID", detail.id],
          ["Path", detail.request_path],
          ["Client", `${detail.client_ip}:${detail.client_port}`],
          ["Server", `${detail.server_ip}:${detail.server_port}`],
          ["Stream", detail.is_stream ? "Yes" : "No"],
          ["Req Time", formatDateTimeMs(detail.request_time)],
        ].map(([k, v]) => (
          <div key={k} className="contents">
            <span className="text-muted-foreground">{k}</span>
            <span className="truncate font-mono text-xs" title={String(v)}>{v}</span>
          </div>
        ))}
      </div>
      <CollapsibleSection title="Request Headers" count={reqH.length}>
        {reqH.length ? (
          <table className="w-full text-sm"><tbody>{reqH.map(([k, v], i) => (
            <tr key={i} className="border-b border-border/30"><td className="w-[200px] py-1 pr-3 font-mono text-xs text-muted-foreground">{k}</td><td className="break-all py-1 font-mono text-xs">{v}</td></tr>
          ))}</tbody></table>
        ) : <p className="text-sm text-muted-foreground">No headers</p>}
      </CollapsibleSection>
      <CollapsibleSection title="Response Headers" count={respH.length}>
        {respH.length ? (
          <table className="w-full text-sm"><tbody>{respH.map(([k, v], i) => (
            <tr key={i} className="border-b border-border/30"><td className="w-[200px] py-1 pr-3 font-mono text-xs text-muted-foreground">{k}</td><td className="break-all py-1 font-mono text-xs">{v}</td></tr>
          ))}</tbody></table>
        ) : <p className="text-sm text-muted-foreground">No headers</p>}
      </CollapsibleSection>
      <CollapsibleSection title="Request Body">
        {detail.request_body ? (
          <pre className="max-h-[400px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">{formatJson(detail.request_body)}</pre>
        ) : <p className="text-sm text-muted-foreground">No body</p>}
      </CollapsibleSection>
      <CollapsibleSection title="Response Body">
        {detail.response_body ? (
          <pre className="max-h-[400px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">{formatJson(detail.response_body)}</pre>
        ) : <p className="text-sm text-muted-foreground">No body</p>}
      </CollapsibleSection>
    </div>
  )
}
```

- [ ] **Step 7.2: Mount drawer in panel**

In `agent-turn-detail-panel.tsx`, add state:

```tsx
const [rawHttpCallId, setRawHttpCallId] = useState<string | null>(null)
```

Pass `onOpenRawHttp={setRawHttpCallId}` into every `CallCard` rendered in `TurnDetailView` (add a prop on `TurnDetailView`).

At the panel root, after the main content, mount:

```tsx
<RawHttpDrawer callId={rawHttpCallId} onClose={() => setRawHttpCallId(null)} />
```

Delete the entire old `LlmCallDetailView` function + its right-pane branch in the main panel — clicking a call no longer replaces the panel, it uses the drawer. Also delete the `selectedCallId` state pair and the `Back to agent turn` row; this click-to-replace path no longer exists.

- [ ] **Step 7.3: Build + dev-server check**

Manual: expand a call → click "View raw HTTP →" → drawer slides in from the right. Close drawer with `✕` → main panel unchanged (same scroll, same expansion state).

---

### Task 8: URL sync (`?call=N&raw=1`)

**Files:**
- Modify: `console/src/hooks/use-turn-url-state.ts`
- Modify: `console/src/pages/agent-turn-detail-panel.tsx`

- [ ] **Step 8.1: Expand `useTurnUrlState`**

```ts
import { useCallback } from "react"
import { useSearchParams } from "react-router"

export function useTurnUrlState() {
  const [params, setParams] = useSearchParams()
  const call = params.get("call") ? Number(params.get("call")) : null
  const raw = params.get("raw") === "1"

  const setCall = useCallback((seq: number | null) => {
    const next = new URLSearchParams(params)
    if (seq == null) next.delete("call")
    else next.set("call", String(seq))
    if (seq == null) next.delete("raw")
    setParams(next, { replace: true })
  }, [params, setParams])

  const setRaw = useCallback((on: boolean) => {
    const next = new URLSearchParams(params)
    if (on) next.set("raw", "1")
    else next.delete("raw")
    setParams(next, { replace: true })
  }, [params, setParams])

  return { call, raw, setCall, setRaw }
}
```

- [ ] **Step 8.2: Wire URL state into panel**

In `AgentTurnDetailPanel`:

```tsx
const { call: urlCall, raw: urlRaw, setCall, setRaw } = useTurnUrlState()

useEffect(() => {
  if (urlCall != null) setActiveSeq(urlCall)
}, [urlCall])

const handleSelect = (seq: number) => {
  setCall(seq)
  setActiveSeq(seq)
  document.getElementById(`call-${seq}`)?.scrollIntoView({ behavior: "smooth", block: "start" })
}

const openRawHttp = (id: string) => {
  const call = calls.find(c => c.id === id)
  if (call) { setCall(call.sequence); setRaw(true); setRawHttpCallId(id) }
}

const closeRawHttp = () => { setRaw(false); setRawHttpCallId(null) }
```

On mount, if `urlRaw && urlCall`, auto-open the drawer:

```tsx
useEffect(() => {
  if (urlRaw && urlCall != null) {
    const call = calls.find(c => c.sequence === urlCall)
    if (call) setRawHttpCallId(call.id)
  }
}, [urlRaw, urlCall, calls])
```

Also auto-expand the target call on mount: extend `CallCard` with `defaultExpanded` when `active === true && urlCall != null` (already supported — just pass the prop).

- [ ] **Step 8.3: Build + dev-server check**

Manual: share a URL with `?call=7&raw=1` in a new tab → panel opens with call #7 scrolled-to/expanded + Raw HTTP drawer open. Closing drawer updates URL (drops `raw=1`). Clicking another call updates `?call=…`.

---

### Task 9: Loading / error / empty / keyboard

**Files:**
- Modify: `console/src/pages/agent-turn-detail-panel.tsx`

- [ ] **Step 9.1: Loading and error branches**

The outer panel already handles `loadingTurn` / `errorTurn`. Ensure it still does after the refactor. For the `useAgentTurnCalls` loading state with turn already loaded, render 3 skeleton placeholder cards instead of blocking the whole pane:

```tsx
{loadingCalls && calls.length === 0 ? (
  <>
    {[0, 1, 2].map((i) => (
      <div key={i} className="h-12 animate-pulse rounded-lg border border-border bg-muted/40" />
    ))}
  </>
) : (
  calls.map(c => <CallCard key={c.id} ... />)
)}
```

- [ ] **Step 9.2: Empty states**

In `TurnDetailView`: if `calls.length === 0` AND `!loadingCalls`, show a single grey line `No calls` between user_input and final_answer; neither card is repressed.

- [ ] **Step 9.3: Keyboard shortcuts**

Add a panel-level `useEffect` that attaches `keydown`:

```tsx
useEffect(() => {
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      if (rawHttpCallId) { closeRawHttp(); return }
      onClose()
      return
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (calls.length === 0) return
      const delta = e.key === "ArrowDown" ? 1 : -1
      const cur = activeSeq ?? 0
      const nextSeq = Math.max(1, Math.min(calls.length, cur + delta))
      handleSelect(nextSeq)
      e.preventDefault()
    }
    if (e.key === "Enter" && activeSeq != null) {
      const el = document.getElementById(`call-${activeSeq}`)
      el?.querySelector("button")?.click()
      e.preventDefault()
    }
  }
  window.addEventListener("keydown", onKey)
  return () => window.removeEventListener("keydown", onKey)
}, [activeSeq, calls.length, rawHttpCallId])
```

- [ ] **Step 9.4: Build + dev-server check**

Manual:
- Open a turn mid-loading → skeleton cards flash before content arrives.
- Open a turn with 0 calls → "No calls" text inline.
- With a turn open: `↑`/`↓` cycles active call; `Enter` toggles expansion; `Esc` closes drawer first, then panel.

---

### Task 10: Phase 1 polish and commit

- [ ] **Step 10.1: Run repo quality gate**

```
cd /Users/timmy/code/netis/TokenScope
just quality ts
```

Fix any lint/type errors before proceeding.

- [ ] **Step 10.2: Remove dead code**

Delete from `agent-turn-detail-panel.tsx` anything that's no longer referenced: old `CallCard` (now in `call-card.tsx`), `TurnGantt`, `TabButton`, `EmptyTabContent`, `LlmCallDetailView`, `CallTimelineBar`, `SummaryCard`, `HeadersTable`, `parseHeaders`, `formatJson`, `MiniTimelineBar`. Imports should be pruned. Re-run `bun run build` to confirm.

- [ ] **Step 10.3: Manual regression pass**

Start `just dev console`. Open 3 different turns covering: (a) simple turn with short user_input + final_answer, (b) long agent turn with 20+ calls and ≥1 slow call, (c) turn with `status=error`. Confirm:
- Top bar + `ⓘ` popover
- Stats cards (Calls, Tokens+$, Duration+slowest, Status)
- Gantt nav bars proportional to turn length; click jumps
- Calls render as inline cards; collapse/expand works; "View raw HTTP" opens drawer
- URL deep-link works: copy URL with `?call=N&raw=1` and open in new tab
- Keyboard: `Esc`, `↑/↓`, `Enter`
- No React warnings in the browser console.

- [ ] **Step 10.4: Commit Phase 1**

```bash
git add docs/superpowers/specs/2026-04-21-turn-detail-redesign-design.md \
        docs/superpowers/plans/2026-04-21-turn-detail-redesign.md \
        console/src/pages/agent-turn-detail-panel.tsx \
        console/src/components/turn-detail/ \
        console/src/hooks/use-turn-url-state.ts
git commit -m "$(cat <<'EOF'
refactor(console): redesign agent turn detail around behavior narrative

Phase 1 of turn-detail redesign (see docs/superpowers/specs/2026-04-21-turn-detail-redesign-design.md):
frontend-only rework. Replaces the 3-tab/2-pane panel with:

- Vertical Gantt sidebar serving as both navigation and time overview
- Inline narrative (user input → call cards → final answer) replacing tabs
- Secondary right-side Raw HTTP drawer for debug-grade HTTP inspection
- ⓘ metadata popover replacing the in-body metadata section
- URL deep-linking via ?call=N&raw=1
- Keyboard navigation (↑/↓/Enter/Esc)

Backend unchanged; parsed content (tool names, text previews, reasoning)
arrives in Phase 2.
EOF
)"
```

---

## Phase 2 — Backend parsers + list enrichment

Backend parses request/response bodies into structured per-call content; list endpoint returns call type, tool-call summaries, and previews; frontend renders them.

### Task 11: Capture test fixtures from real DB bodies

**Files:**
- Create: `scripts/dump-call-bodies.sh`
- Create: `server/ts-llm/tests/fixtures/anthropic_output_*.json` (text-only, tool-use, thinking, error)
- Create: `server/ts-llm/tests/fixtures/anthropic_input_*.json` (user-only, with-tool-result)
- Create: `server/ts-llm/tests/fixtures/openai_chat_output_*.json`, `openai_chat_input_*.json`
- Create: `server/ts-llm/tests/fixtures/openai_responses_output_*.json`, `openai_responses_input_*.json`

- [ ] **Step 11.1: Dump helper**

```bash
#!/usr/bin/env bash
# scripts/dump-call-bodies.sh
# Usage: scripts/dump-call-bodies.sh <path-to-duckdb-file> <wire_api> <output|input> <out-dir>
set -euo pipefail
DB="$1"; WIRE="$2"; SIDE="$3"; OUT="$4"
mkdir -p "$OUT"
FIELD="response_body"
[ "$SIDE" = "input" ] && FIELD="request_body"
duckdb "$DB" -noheader -list -cmd ".mode json" \
  "SELECT id, $FIELD FROM llm_calls WHERE wire_api = '$WIRE' AND $FIELD IS NOT NULL LIMIT 5" \
  | jq -c '.[]' \
  | while IFS= read -r row; do
      id=$(jq -r '.id' <<<"$row")
      body=$(jq -r ".${FIELD}" <<<"$row")
      printf '%s' "$body" > "$OUT/${WIRE}_${SIDE}_${id}.json"
    done
echo "wrote to $OUT"
```

Make executable:

```
chmod +x scripts/dump-call-bodies.sh
```

- [ ] **Step 11.2: Hand-curate minimum fixtures**

Each parser test later needs at least these shapes. Capture one real row per shape using the dump script or by handcrafting. Place under `server/ts-llm/tests/fixtures/`:

For Anthropic:
- `anthropic_output_text_only.json` — response with `content: [{type:"text", text:"..."}]`, `stop_reason:"end_turn"`
- `anthropic_output_tool_use.json` — response with `content: [{type:"text"}, {type:"tool_use", id:"toolu_X", name:"read_file", input:{...}}]`, `stop_reason:"tool_use"`
- `anthropic_output_thinking.json` — response with `content: [{type:"thinking", thinking:"..."}, {type:"text"}]`
- `anthropic_input_user_only.json` — `messages: [{role:"user", content:"hi"}]`
- `anthropic_input_with_tool_result.json` — `messages: [..., {role:"user", content:[{type:"tool_result", tool_use_id:"toolu_X", content:"..."}]}]`

For OpenAI Chat:
- `openai_chat_output_text.json` — `{choices:[{message:{content:"..."}}]}`
- `openai_chat_output_tool_calls.json` — `{choices:[{message:{content:null, tool_calls:[{id:"call_X", function:{name,arguments}}]}}]}`
- `openai_chat_input_with_tool_result.json` — `{messages:[..., {role:"tool", tool_call_id:"call_X", content:"..."}]}`

For OpenAI Responses:
- `openai_responses_output_message.json` — `{output:[{type:"message", content:[{type:"output_text", text:"..."}]}]}`
- `openai_responses_output_function_call.json` — `{output:[{type:"reasoning", summary:[{text:"..."}]}, {type:"function_call", call_id:"...", name, arguments}]}`
- `openai_responses_input_with_function_call_output.json` — `{input:[..., {type:"function_call_output", call_id:"...", output:"..."}]}`

**Verification:** each file parses as valid JSON:

```
find server/ts-llm/tests/fixtures -name '*.json' -exec jq empty {} \;
```

Expected: no output (success).

---

### Task 12: Add `ParsedOutput` / `ParsedInput` / `ParsedToolCall` / `ParsedToolResult` types

**Files:**
- Modify: `server/ts-llm/src/model.rs`

- [ ] **Step 12.1: Add types at the end of `model.rs` (before the test mod if present)**

```rust
/// Structured view of an LLM output extracted from a response body.
/// Per-wire-api implementations of `WireApi::parse_output` produce this.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedOutput {
    pub reasoning: Option<String>,
    pub message: Option<String>,
    pub tool_calls: Vec<ParsedToolCall>,
}

/// Structured view of an LLM input extracted from a request body.
/// Per-wire-api implementations of `WireApi::parse_input` produce this.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedInput {
    /// Most-recent user message in the input, if any.
    pub user_message: Option<String>,
    /// Tool results keyed by the `id` / `call_id` they belong to.
    pub tool_results: Vec<ParsedToolResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}
```

- [ ] **Step 12.2: Build check**

```
cargo check -p ts-llm
```

Expected: PASS (no new errors; existing warnings unchanged).

---

### Task 13: Extend `WireApi` trait with parse methods

**Files:**
- Modify: `server/ts-llm/src/model.rs`

- [ ] **Step 13.1: Add two default-empty methods to the `WireApi` trait**

In `model.rs`, extend the `WireApi` trait:

```rust
    /// Structured view of the output: reasoning / message / tool_calls.
    /// Default returns an empty `ParsedOutput`; concrete wire APIs should override.
    fn parse_output(&self, _body: &serde_json::Value) -> ParsedOutput {
        ParsedOutput::default()
    }

    /// Structured view of the input: most-recent user message + tool_results.
    /// Default returns an empty `ParsedInput`; concrete wire APIs should override.
    fn parse_input(&self, _body: &serde_json::Value) -> ParsedInput {
        ParsedInput::default()
    }
```

- [ ] **Step 13.2: Build check**

```
cargo check -p ts-llm
cargo build -p ts-api   # ensures nothing downstream breaks
```

---

### Task 14: Anthropic `parse_output` (TDD)

**Files:**
- Modify: `server/ts-llm/src/wire_apis/anthropic.rs`

- [ ] **Step 14.1: Add failing test**

In the existing `#[cfg(test)] mod tests` at the bottom of `anthropic.rs`, add:

```rust
    #[test]
    fn parse_output_text_only() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/anthropic_output_text_only.json"),
        )
        .unwrap();
        let out = AnthropicWireApi.parse_output(&body);
        assert_eq!(out.reasoning, None);
        assert!(out.message.as_deref().map(|s| !s.is_empty()).unwrap_or(false));
        assert!(out.tool_calls.is_empty());
    }

    #[test]
    fn parse_output_tool_use() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/anthropic_output_tool_use.json"),
        )
        .unwrap();
        let out = AnthropicWireApi.parse_output(&body);
        assert_eq!(out.tool_calls.len(), 1);
        let tc = &out.tool_calls[0];
        assert!(tc.id.starts_with("toolu_"));
        assert_eq!(tc.name, "read_file");
        assert!(tc.args_json.contains("\"path\""));
    }

    #[test]
    fn parse_output_thinking() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/anthropic_output_thinking.json"),
        )
        .unwrap();
        let out = AnthropicWireApi.parse_output(&body);
        assert!(out.reasoning.as_deref().map(|s| !s.is_empty()).unwrap_or(false));
        assert!(out.message.as_deref().map(|s| !s.is_empty()).unwrap_or(false));
    }
```

- [ ] **Step 14.2: Run — expect FAIL**

```
cargo test -p ts-llm parse_output_ -- --nocapture
```

Expected: all three tests FAIL (default trait impl returns empty `ParsedOutput`).

- [ ] **Step 14.3: Implement**

Add to `impl WireApi for AnthropicWireApi` in `anthropic.rs`:

```rust
    fn parse_output(&self, body: &Value) -> crate::model::ParsedOutput {
        use crate::model::{ParsedOutput, ParsedToolCall};
        let mut out = ParsedOutput::default();
        let Some(content) = body.get("content").and_then(|v| v.as_array()) else {
            return out;
        };
        let mut reasoning_buf = String::new();
        let mut message_buf = String::new();
        for block in content {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                        if !reasoning_buf.is_empty() { reasoning_buf.push('\n'); }
                        reasoning_buf.push_str(t);
                    }
                }
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        if !message_buf.is_empty() { message_buf.push('\n'); }
                        message_buf.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let args_json = block.get("input")
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .unwrap_or_default();
                    out.tool_calls.push(ParsedToolCall { id, name, args_json });
                }
                _ => {}
            }
        }
        if !reasoning_buf.is_empty() { out.reasoning = Some(reasoning_buf); }
        if !message_buf.is_empty() { out.message = Some(message_buf); }
        out
    }
```

- [ ] **Step 14.4: Run — expect PASS**

```
cargo test -p ts-llm parse_output_ -- --nocapture
```

Expected: 3 passed.

---

### Task 15: Anthropic `parse_input` (TDD)

**Files:**
- Modify: `server/ts-llm/src/wire_apis/anthropic.rs`

- [ ] **Step 15.1: Add failing tests**

```rust
    #[test]
    fn parse_input_user_only() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/anthropic_input_user_only.json"),
        )
        .unwrap();
        let out = AnthropicWireApi.parse_input(&body);
        assert!(out.user_message.is_some());
        assert!(out.tool_results.is_empty());
    }

    #[test]
    fn parse_input_with_tool_result() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/anthropic_input_with_tool_result.json"),
        )
        .unwrap();
        let out = AnthropicWireApi.parse_input(&body);
        assert_eq!(out.tool_results.len(), 1);
        assert!(out.tool_results[0].tool_use_id.starts_with("toolu_"));
        assert!(!out.tool_results[0].content.is_empty());
    }
```

- [ ] **Step 15.2: Run — expect FAIL**

```
cargo test -p ts-llm parse_input_ -- --nocapture
```

Expected: FAIL.

- [ ] **Step 15.3: Implement**

```rust
    fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
        use crate::model::{ParsedInput, ParsedToolResult};
        let mut out = ParsedInput::default();
        let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
            return out;
        };
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content");
            // Content can be a string OR an array of blocks.
            if let Some(s) = content.and_then(|v| v.as_str()) {
                if role == "user" { out.user_message = Some(s.to_string()); }
                continue;
            }
            if let Some(arr) = content.and_then(|v| v.as_array()) {
                let mut user_text_buf = String::new();
                for block in arr {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") if role == "user" => {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                if !user_text_buf.is_empty() { user_text_buf.push('\n'); }
                                user_text_buf.push_str(t);
                            }
                        }
                        Some("tool_result") => {
                            let tool_use_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let is_error = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                            let content_str = match block.get("content") {
                                Some(c) if c.is_string() => c.as_str().unwrap().to_string(),
                                Some(c) if c.is_array() => {
                                    c.as_array().unwrap().iter().filter_map(|b| b.get("text").and_then(|v| v.as_str())).collect::<Vec<_>>().join("\n")
                                }
                                Some(c) => serde_json::to_string(c).unwrap_or_default(),
                                None => String::new(),
                            };
                            out.tool_results.push(ParsedToolResult { tool_use_id, content: content_str, is_error });
                        }
                        _ => {}
                    }
                }
                if role == "user" && !user_text_buf.is_empty() {
                    // Later user messages override earlier ones — the parser returns the most recent.
                    out.user_message = Some(user_text_buf);
                }
            }
        }
        out
    }
```

- [ ] **Step 15.4: Run — expect PASS**

```
cargo test -p ts-llm parse_input_ -- --nocapture
```

Expected: 2 passed.

---

### Task 16: OpenAI Chat `parse_output` + `parse_input` (TDD)

**Files:**
- Modify: `server/ts-llm/src/wire_apis/openai.rs`

- [ ] **Step 16.1: Add failing tests**

Inside the existing `#[cfg(test)] mod tests` in `openai.rs`:

```rust
    #[test]
    fn chat_parse_output_text() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/openai_chat_output_text.json"),
        ).unwrap();
        let out = OpenAiChatWireApi.parse_output(&body);
        assert!(out.message.as_deref().map(|s| !s.is_empty()).unwrap_or(false));
        assert!(out.tool_calls.is_empty());
    }

    #[test]
    fn chat_parse_output_tool_calls() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/openai_chat_output_tool_calls.json"),
        ).unwrap();
        let out = OpenAiChatWireApi.parse_output(&body);
        assert_eq!(out.tool_calls.len(), 1);
        assert!(out.tool_calls[0].id.starts_with("call_"));
    }

    #[test]
    fn chat_parse_input_tool_result() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/openai_chat_input_with_tool_result.json"),
        ).unwrap();
        let out = OpenAiChatWireApi.parse_input(&body);
        assert_eq!(out.tool_results.len(), 1);
        assert!(out.tool_results[0].tool_use_id.starts_with("call_"));
    }
```

- [ ] **Step 16.2: Run — expect FAIL**

```
cargo test -p ts-llm chat_parse_ -- --nocapture
```

- [ ] **Step 16.3: Implement**

In `impl WireApi for OpenAiChatWireApi` in `openai.rs`:

```rust
    fn parse_output(&self, body: &Value) -> crate::model::ParsedOutput {
        use crate::model::{ParsedOutput, ParsedToolCall};
        let mut out = ParsedOutput::default();
        let Some(msg) = body.get("choices")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message")) else { return out; };
        if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
            if !c.is_empty() { out.message = Some(c.to_string()); }
        }
        // Newer APIs also expose `reasoning_content` on reasoning models.
        if let Some(r) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
            if !r.is_empty() { out.reasoning = Some(r.to_string()); }
        }
        if let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in arr {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let function = tc.get("function");
                let name = function.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args_json = function.and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                out.tool_calls.push(ParsedToolCall { id, name, args_json });
            }
        }
        out
    }

    fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
        use crate::model::{ParsedInput, ParsedToolResult};
        let mut out = ParsedInput::default();
        let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else { return out; };
        for msg in messages {
            match msg.get("role").and_then(|v| v.as_str()) {
                Some("user") => {
                    if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
                        out.user_message = Some(c.to_string());
                    } else if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
                        let text = arr.iter()
                            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() { out.user_message = Some(text); }
                    }
                }
                Some("tool") => {
                    let tool_use_id = msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let content = msg.get("content").and_then(|v| v.as_str()).map(|s| s.to_string())
                        .or_else(|| msg.get("content").map(|v| v.to_string()))
                        .unwrap_or_default();
                    out.tool_results.push(ParsedToolResult { tool_use_id, content, is_error: false });
                }
                _ => {}
            }
        }
        out
    }
```

- [ ] **Step 16.4: Run — expect PASS**

```
cargo test -p ts-llm chat_parse_ -- --nocapture
```

---

### Task 17: OpenAI Responses `parse_output` + `parse_input` (TDD)

**Files:**
- Modify: `server/ts-llm/src/wire_apis/openai.rs`

- [ ] **Step 17.1: Add failing tests**

```rust
    #[test]
    fn responses_parse_output_message() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/openai_responses_output_message.json"),
        ).unwrap();
        let out = OpenAiResponsesWireApi.parse_output(&body);
        assert!(out.message.as_deref().map(|s| !s.is_empty()).unwrap_or(false));
        assert!(out.tool_calls.is_empty());
    }

    #[test]
    fn responses_parse_output_function_call_with_reasoning() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/openai_responses_output_function_call.json"),
        ).unwrap();
        let out = OpenAiResponsesWireApi.parse_output(&body);
        assert!(out.reasoning.is_some());
        assert_eq!(out.tool_calls.len(), 1);
    }

    #[test]
    fn responses_parse_input_with_function_call_output() {
        let body: serde_json::Value = serde_json::from_str(
            include_str!("../../tests/fixtures/openai_responses_input_with_function_call_output.json"),
        ).unwrap();
        let out = OpenAiResponsesWireApi.parse_input(&body);
        assert_eq!(out.tool_results.len(), 1);
    }
```

- [ ] **Step 17.2: Run — expect FAIL**

```
cargo test -p ts-llm responses_parse_ -- --nocapture
```

- [ ] **Step 17.3: Implement**

In `impl WireApi for OpenAiResponsesWireApi`:

```rust
    fn parse_output(&self, body: &Value) -> crate::model::ParsedOutput {
        use crate::model::{ParsedOutput, ParsedToolCall};
        let mut out = ParsedOutput::default();
        let Some(items) = body.get("output").and_then(|v| v.as_array()) else { return out; };
        let mut reasoning_buf = String::new();
        let mut message_buf = String::new();
        for item in items {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("reasoning") => {
                    // `summary` is an array of { text } in current Responses schema.
                    if let Some(arr) = item.get("summary").and_then(|v| v.as_array()) {
                        for s in arr {
                            if let Some(t) = s.get("text").and_then(|v| v.as_str()) {
                                if !reasoning_buf.is_empty() { reasoning_buf.push('\n'); }
                                reasoning_buf.push_str(t);
                            }
                        }
                    }
                }
                Some("message") => {
                    if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
                        for part in arr {
                            if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                    if !message_buf.is_empty() { message_buf.push('\n'); }
                                    message_buf.push_str(t);
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let args_json = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    out.tool_calls.push(ParsedToolCall { id, name, args_json });
                }
                _ => {}
            }
        }
        if !reasoning_buf.is_empty() { out.reasoning = Some(reasoning_buf); }
        if !message_buf.is_empty() { out.message = Some(message_buf); }
        out
    }

    fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
        use crate::model::{ParsedInput, ParsedToolResult};
        let mut out = ParsedInput::default();
        // `input` may be a string OR an array of items.
        if let Some(s) = body.get("input").and_then(|v| v.as_str()) {
            out.user_message = Some(s.to_string());
            return out;
        }
        let Some(items) = body.get("input").and_then(|v| v.as_array()) else { return out; };
        for item in items {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("message") if item.get("role").and_then(|v| v.as_str()) == Some("user") => {
                    let content = item.get("content");
                    if let Some(s) = content.and_then(|v| v.as_str()) {
                        out.user_message = Some(s.to_string());
                    } else if let Some(arr) = content.and_then(|v| v.as_array()) {
                        let text = arr.iter()
                            .filter_map(|b| match b.get("type").and_then(|v| v.as_str()) {
                                Some("input_text") | Some("text") => b.get("text").and_then(|v| v.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() { out.user_message = Some(text); }
                    }
                }
                Some("function_call_output") => {
                    let tool_use_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let content = item.get("output").and_then(|v| v.as_str()).map(|s| s.to_string())
                        .or_else(|| item.get("output").map(|v| v.to_string()))
                        .unwrap_or_default();
                    out.tool_results.push(ParsedToolResult { tool_use_id, content, is_error: false });
                }
                _ => {}
            }
        }
        out
    }
```

- [ ] **Step 17.4: Run — expect PASS**

```
cargo test -p ts-llm responses_parse_ -- --nocapture
```

- [ ] **Step 17.5: Run full ts-llm tests to catch regressions**

```
cargo test -p ts-llm
```

Expected: all tests pass.

---

### Task 18: Extend storage `TurnCallItem` with body fields + update SQL

**Files:**
- Modify: `server/ts-storage/src/query.rs`
- Modify: `server/ts-storage/src/duckdb.rs`
- Modify: `server/ts-storage/src/sink.rs` (test double — update if needed)

- [ ] **Step 18.1: Add body fields to `TurnCallItem`**

In `server/ts-storage/src/query.rs` around line 227:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct TurnCallItem {
    pub id: String,
    pub sequence: u32,
    pub request_time: i64,
    pub response_time: Option<i64>,
    pub complete_time: Option<i64>,
    pub wire_api: String,
    pub model: String,
    pub status_code: Option<u16>,
    pub is_stream: bool,
    pub finish_reason: Option<String>,
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    /// Only populated by `query_turn_calls` (for in-API-layer parsing).
    /// Not serialized to the API response — the route strips it after parsing.
    #[serde(skip)]
    pub request_body: Option<String>,
    #[serde(skip)]
    pub response_body: Option<String>,
}
```

- [ ] **Step 18.2: Update the DuckDB SQL**

In `server/ts-storage/src/duckdb.rs` around line 2184, modify the SQL in `query_turn_calls` to SELECT bodies, and update the row-read loop:

```rust
            let sql = "
                SELECT
                    c.id,
                    epoch_ms(c.request_time),
                    epoch_ms(c.response_time),
                    epoch_ms(c.complete_time),
                    c.wire_api, c.model, c.status_code, c.is_stream,
                    c.finish_reason, c.ttfb_ms, c.e2e_latency_ms,
                    c.input_tokens, c.output_tokens,
                    c.request_body, c.response_body
                FROM llm_calls c
                JOIN (SELECT UNNEST(json_extract_string(call_ids, '$[*]')) AS cid
                      FROM agent_turns WHERE turn_id = ?) ids ON c.id = ids.cid
                ORDER BY c.request_time ASC, c.complete_time ASC
            ";
```

In the row read loop, after the existing column reads, add two more columns at indices 13 and 14:

```rust
                    input_tokens: row.get(11).map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    output_tokens: row.get(12).map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_body: row.get(13).ok(),
                    response_body: row.get(14).ok(),
                });
```

- [ ] **Step 18.3: Update test double in `sink.rs` if it constructs `TurnCallItem`**

```
grep -n "TurnCallItem {" server/ts-storage/src/sink.rs
```

If results come back, add `request_body: None, response_body: None` to each literal.

- [ ] **Step 18.4: Run storage tests**

```
cargo test -p ts-storage
```

Expected: all pass. The existing `query_turn_calls_orders_and_sequences` test should still pass — bodies simply come through as `None` for rows without them.

---

### Task 19: Extend `TurnCallItem` in the API response with parsed fields

**Files:**
- Create: `server/ts-api/src/routes/turn_call_enrichment.rs`
- Modify: `server/ts-api/src/routes/agent_turns.rs`
- Modify: `server/ts-api/src/routes/mod.rs`

- [ ] **Step 19.1: Create the enrichment module**

`server/ts-api/src/routes/turn_call_enrichment.rs`:

```rust
use serde::Serialize;
use ts_llm::model::{ParsedInput, ParsedOutput, WireApi};
use ts_llm::wire_api_registry::WireApiRegistry;
use ts_storage::query::TurnCallItem;

const ARGS_PREVIEW_LEN: usize = 200;
const REASONING_PREVIEW_LEN: usize = 120;
const MESSAGE_PREVIEW_LEN: usize = 60;

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedTurnCallItem {
    // Existing fields (flattened from TurnCallItem).
    pub id: String,
    pub sequence: u32,
    pub request_time: i64,
    pub response_time: Option<i64>,
    pub complete_time: Option<i64>,
    pub wire_api: String,
    pub model: String,
    pub status_code: Option<u16>,
    pub is_stream: bool,
    pub finish_reason: Option<String>,
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,

    // New Phase 2 fields.
    pub r#type: &'static str, // "tool_call" | "text" | "final"
    pub tool_calls: Vec<EnrichedToolCall>,
    pub has_reasoning: bool,
    pub reasoning_preview: Option<String>,
    pub message_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedToolCall {
    pub id: String,
    pub name: String,
    pub args_preview: String,
    pub result_summary: Option<ResultSummary>,   // populated in Phase 3
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultSummary {
    pub size_bytes: u64,
    pub kind: &'static str,
    pub is_error: bool,
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>()
    }
}

pub fn enrich(
    items: Vec<TurnCallItem>,
    final_call_id: Option<&str>,
    registry: &WireApiRegistry,
) -> Vec<EnrichedTurnCallItem> {
    items
        .into_iter()
        .map(|c| {
            let wire = registry.find_by_name(&c.wire_api);
            let (parsed_out, _parsed_in) = wire
                .map(|w| parse_bodies(w, c.request_body.as_deref(), c.response_body.as_deref()))
                .unwrap_or((ParsedOutput::default(), ParsedInput::default()));

            let is_final = final_call_id.map(|f| f == c.id).unwrap_or(false);
            let type_str: &'static str = if is_final {
                "final"
            } else if !parsed_out.tool_calls.is_empty() {
                "tool_call"
            } else {
                "text"
            };

            let tool_calls = parsed_out
                .tool_calls
                .into_iter()
                .map(|tc| EnrichedToolCall {
                    id: tc.id,
                    name: tc.name,
                    args_preview: truncate(&tc.args_json, ARGS_PREVIEW_LEN),
                    result_summary: None,
                })
                .collect();

            EnrichedTurnCallItem {
                id: c.id,
                sequence: c.sequence,
                request_time: c.request_time,
                response_time: c.response_time,
                complete_time: c.complete_time,
                wire_api: c.wire_api,
                model: c.model,
                status_code: c.status_code,
                is_stream: c.is_stream,
                finish_reason: c.finish_reason,
                ttfb_ms: c.ttfb_ms,
                e2e_latency_ms: c.e2e_latency_ms,
                input_tokens: c.input_tokens,
                output_tokens: c.output_tokens,

                r#type: type_str,
                tool_calls,
                has_reasoning: parsed_out.reasoning.is_some(),
                reasoning_preview: parsed_out.reasoning.map(|s| truncate(&s, REASONING_PREVIEW_LEN)),
                message_preview: parsed_out.message.map(|s| truncate(&s, MESSAGE_PREVIEW_LEN)),
            }
        })
        .collect()
}

fn parse_bodies(
    wire: &dyn WireApi,
    req_body: Option<&str>,
    resp_body: Option<&str>,
) -> (ParsedOutput, ParsedInput) {
    let resp_val = resp_body.and_then(|s| serde_json::from_str(s).ok()).unwrap_or(serde_json::Value::Null);
    let req_val = req_body.and_then(|s| serde_json::from_str(s).ok()).unwrap_or(serde_json::Value::Null);
    (wire.parse_output(&resp_val), wire.parse_input(&req_val))
}
```

(The registry already exposes `find_by_name`; use that.)

- [ ] **Step 19.2: Wire into the calls route**

In `server/ts-api/src/routes/agent_turns.rs`:

```rust
use super::turn_call_enrichment::enrich;
use ts_llm::wire_apis::build_default_wire_api_registry;

pub async fn calls(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path(turn_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let items = storage.query_turn_calls(&turn_id).await?;
    let turn = storage.query_turn_by_id(&turn_id).await?;
    let final_call_id = turn.as_ref().and_then(|t| t.final_call_id.as_deref());
    let registry = build_default_wire_api_registry();
    let enriched = enrich(items, final_call_id, &registry);
    Ok(ApiResponse::ok(enriched))
}
```

- [ ] **Step 19.3: Register the new module**

In `server/ts-api/src/routes/mod.rs`, add `pub mod turn_call_enrichment;`.

- [ ] **Step 19.4: Add a unit test for `enrich`**

In `server/ts-api/src/routes/turn_call_enrichment.rs`, add at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ts_llm::wire_apis::build_default_wire_api_registry;
    use ts_storage::query::TurnCallItem;

    fn anthropic_tool_use_body() -> String {
        r#"{"content":[{"type":"text","text":"let me check"},{"type":"tool_use","id":"toolu_abc","name":"read_file","input":{"path":"x"}}],"stop_reason":"tool_use"}"#.to_string()
    }

    fn mk_item(id: &str, wire: &str, body: &str) -> TurnCallItem {
        TurnCallItem {
            id: id.into(), sequence: 1, request_time: 0,
            response_time: None, complete_time: None,
            wire_api: wire.into(), model: "claude".into(),
            status_code: Some(200), is_stream: false,
            finish_reason: Some("tool_use".into()),
            ttfb_ms: None, e2e_latency_ms: Some(1000.0),
            input_tokens: None, output_tokens: None,
            request_body: None, response_body: Some(body.into()),
        }
    }

    #[test]
    fn enrich_marks_tool_call_type() {
        let reg = build_default_wire_api_registry();
        let items = vec![mk_item("c1", "anthropic", &anthropic_tool_use_body())];
        let enriched = enrich(items, None, &reg);
        assert_eq!(enriched[0].r#type, "tool_call");
        assert_eq!(enriched[0].tool_calls.len(), 1);
        assert_eq!(enriched[0].tool_calls[0].name, "read_file");
    }

    #[test]
    fn enrich_marks_final_by_id() {
        let reg = build_default_wire_api_registry();
        let items = vec![mk_item("c1", "anthropic", &anthropic_tool_use_body())];
        let enriched = enrich(items, Some("c1"), &reg);
        assert_eq!(enriched[0].r#type, "final");
    }
}
```

Run: `cargo test -p ts-api enrich_ -- --nocapture`
Expected: 2 passed.

- [ ] **Step 19.5: Build + manual API check**

```
cargo build -p ts-api
```

Manual: start the backend (`just dev server`) against a DB with real turns. `curl http://localhost:8080/api/agent-turns/<known-turn-id>/calls | jq '.data[0] | {type, tool_calls: .tool_calls | length, message_preview, has_reasoning}'`. Expected: `type` is one of `tool_call|text|final`; `tool_calls` length > 0 for tool-heavy calls; `message_preview` truncated to ≤60 chars.

---

### Task 20: Frontend type + rendering updates

**Files:**
- Modify: `console/src/types/api.ts`
- Modify: `console/src/components/turn-detail/call-card.tsx`
- Modify: `console/src/components/turn-detail/gantt-nav.tsx`
- Modify: `console/src/components/turn-detail/stats-cards.tsx`

- [ ] **Step 20.1: Extend TS types**

In `console/src/types/api.ts`, replace `AgentTurnCallItem`:

```ts
export type CallType = "tool_call" | "text" | "final"

export interface EnrichedToolCall {
  id: string
  name: string
  args_preview: string
  result_summary: {
    size_bytes: number
    kind: "text" | "json" | "error" | "binary" | "missing"
    is_error: boolean
  } | null
}

export interface AgentTurnCallItem {
  id: string
  sequence: number
  request_time: number
  response_time: number | null
  complete_time: number | null
  wire_api: string
  model: string
  status_code: number | null
  is_stream: boolean
  finish_reason: string | null
  ttfb_ms: number | null
  e2e_latency_ms: number | null
  input_tokens: number | null
  output_tokens: number | null

  // Phase 2+
  type: CallType
  tool_calls: EnrichedToolCall[]
  has_reasoning: boolean
  reasoning_preview: string | null
  message_preview: string | null
}
```

- [ ] **Step 20.2: Use real type icons in `gantt-nav.tsx`**

Replace `TypeIcon`:

```tsx
import { Wrench, MessageSquare, Target } from "lucide-react"
function TypeIcon({ call }: { call: AgentTurnCallItem }) {
  const cls = "size-3"
  if (call.type === "tool_call") return <Wrench className={cn(cls, "text-amber-600")} />
  if (call.type === "final")     return <Target className={cn(cls, "text-emerald-600")} />
  return <MessageSquare className={cn(cls, "text-blue-600")} />
}
```

- [ ] **Step 20.3: Render chips + previews in `call-card.tsx`**

Replace the collapsed-row inner with:

```tsx
import { Wrench, MessageSquare, Target } from "lucide-react"

function TypeChip({ call }: { call: AgentTurnCallItem }) {
  if (call.type === "tool_call") {
    const names = call.tool_calls.slice(0, 2).map(t => t.name)
    const more = call.tool_calls.length - names.length
    return (
      <span className="flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-300">
        <Wrench className="size-3" />
        {names.join(", ")}
        {more > 0 && <span className="ml-1 opacity-70">+{more}</span>}
      </span>
    )
  }
  if (call.type === "final") {
    return (
      <span className="flex items-center gap-1 rounded bg-emerald-100 px-1.5 py-0.5 text-[10px] font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300">
        <Target className="size-3" /> final
      </span>
    )
  }
  return (
    <span className="flex items-center gap-1 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
      <MessageSquare className="size-3" /> text
    </span>
  )
}
```

Update the `CallCard` header to render `<TypeChip>` in place of `<FinishBadge>` (FinishBadge still appears in the expanded meta row), and add a preview line below when present:

```tsx
<div className="flex w-full flex-col gap-1 px-3 py-2 text-left">
  <div className="flex items-center gap-3">
    <span className="w-6 shrink-0 tabular-nums text-xs text-muted-foreground">#{call.sequence}</span>
    <TypeChip call={call} />
    <span className="flex-1 truncate text-xs text-muted-foreground">{call.model}</span>
    {/* ...existing duration + tokens + chevron */}
  </div>
  {(call.message_preview ?? call.tool_calls[0]?.args_preview) && (
    <div className="truncate pl-9 text-[11px] text-muted-foreground">
      {call.message_preview
        ? `"${call.message_preview}${(call.message_preview?.length ?? 0) >= 60 ? "…" : ""}"`
        : call.tool_calls[0].args_preview}
    </div>
  )}
</div>
```

- [ ] **Step 20.4: Turn on Stats "Calls" breakdown**

In `stats-cards.tsx`, compute counts per type and add the breakdown line under the Calls count:

```tsx
const typeCounts = useMemo(() => {
  const acc = { tool_call: 0, text: 0, final: 0 }
  for (const c of calls) acc[c.type]++
  return acc
}, [calls])

// Inside the Calls card:
<div className="flex items-center gap-2 text-[10px] text-muted-foreground">
  <span className="inline-flex items-center gap-0.5"><Wrench className="size-2.5" />{typeCounts.tool_call}</span>
  <span className="inline-flex items-center gap-0.5"><MessageSquare className="size-2.5" />{typeCounts.text}</span>
  <span className="inline-flex items-center gap-0.5"><Target className="size-2.5" />{typeCounts.final}</span>
</div>
```

- [ ] **Step 20.5: Build + dev-server check**

```
cd console && bun run build
```

Manual: reload a turn detail. Expect tool names in chips, message previews under each call, type breakdown in Calls card, colored icons in the Gantt sidebar.

---

### Task 21: Phase 2 polish and commit

- [ ] **Step 21.1: Run both quality gates**

```
just quality rs
just quality ts
```

Fix any fallout.

- [ ] **Step 21.2: Full test run**

```
cargo test --workspace
```

Expected: all pass.

- [ ] **Step 21.3: Commit Phase 2**

```bash
git add server/ts-llm/src/model.rs \
        server/ts-llm/src/wire_apis/anthropic.rs \
        server/ts-llm/src/wire_apis/openai.rs \
        server/ts-llm/src/wire_api_registry.rs \
        server/ts-llm/tests/fixtures/ \
        server/ts-storage/src/query.rs \
        server/ts-storage/src/duckdb.rs \
        server/ts-storage/src/sink.rs \
        server/ts-api/src/routes/mod.rs \
        server/ts-api/src/routes/agent_turns.rs \
        server/ts-api/src/routes/turn_call_enrichment.rs \
        console/src/types/api.ts \
        console/src/components/turn-detail/ \
        scripts/dump-call-bodies.sh
git commit -m "$(cat <<'EOF'
feat(turn-detail): parse call bodies and enrich /calls payload

Phase 2 of turn-detail redesign. Extends the WireApi trait with
parse_output / parse_input on all three supported wire APIs
(anthropic, openai-chat, openai-responses). The /api/agent-turns/:id/calls
endpoint now returns per-call type, tool_calls[], reasoning_preview,
and message_preview. Frontend renders tool chips, text previews,
and a type breakdown in the Calls stats card.
EOF
)"
```

---

## Phase 3 — Full parsed detail + tool-result join

### Task 22: `attach_tool_results` in ts-turn (TDD)

**Files:**
- Create: `server/ts-turn/src/tool_result_join.rs`
- Modify: `server/ts-turn/src/lib.rs` — `pub mod tool_result_join;`

- [ ] **Step 22.1: Write failing tests**

`server/ts-turn/src/tool_result_join.rs`:

```rust
//! Join tool_use blocks from call N to tool_result blocks from call N+1.
//!
//! The backend parses each call's request/response body into `ParsedInput`
//! and `ParsedOutput` (see ts-llm::parse). Tool-use blocks are emitted by
//! call N's output; their corresponding tool_results live in call N+1's
//! input (indexed by tool_use_id). This module walks adjacent pairs and
//! attaches each result to its call.

use ts_llm::model::{ParsedInput, ParsedOutput, ParsedToolResult};

/// Walk adjacent call pairs and attach each tool_use's result (if any) from
/// the successor call's input. A call that has no successor leaves results as
/// `None` — the UI renders "(no response, turn ended)".
pub fn attach_tool_results<'a>(
    outputs: &'a [ParsedOutput],
    inputs: &'a [ParsedInput],
) -> Vec<Vec<(String /*tool_use_id*/, Option<&'a ParsedToolResult>)>> {
    assert_eq!(outputs.len(), inputs.len(), "outputs and inputs must be 1-1 and sorted by call sequence");
    outputs
        .iter()
        .enumerate()
        .map(|(i, out)| {
            let next_input = inputs.get(i + 1);
            out.tool_calls
                .iter()
                .map(|tc| {
                    let r = next_input
                        .and_then(|ni| ni.tool_results.iter().find(|tr| tr.tool_use_id == tc.id));
                    (tc.id.clone(), r)
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts_llm::model::{ParsedInput, ParsedOutput, ParsedToolCall, ParsedToolResult};

    fn out_with_tc(id: &str) -> ParsedOutput {
        ParsedOutput {
            reasoning: None,
            message: None,
            tool_calls: vec![ParsedToolCall { id: id.into(), name: "x".into(), args_json: "{}".into() }],
        }
    }

    fn input_with_tr(id: &str) -> ParsedInput {
        ParsedInput {
            user_message: None,
            tool_results: vec![ParsedToolResult { tool_use_id: id.into(), content: "ok".into(), is_error: false }],
        }
    }

    #[test]
    fn attaches_result_from_next_call() {
        let outs = vec![out_with_tc("tc1"), ParsedOutput::default()];
        let ins = vec![ParsedInput::default(), input_with_tr("tc1")];
        let joined = attach_tool_results(&outs, &ins);
        assert_eq!(joined[0].len(), 1);
        assert_eq!(joined[0][0].0, "tc1");
        assert!(joined[0][0].1.is_some());
    }

    #[test]
    fn last_call_tool_use_has_no_result() {
        let outs = vec![out_with_tc("tc_orphan")];
        let ins = vec![ParsedInput::default()];
        let joined = attach_tool_results(&outs, &ins);
        assert_eq!(joined[0][0].1.map(|_| ()), None);
    }

    #[test]
    fn mismatched_id_returns_none() {
        let outs = vec![out_with_tc("tc1"), ParsedOutput::default()];
        let ins = vec![ParsedInput::default(), input_with_tr("other")];
        let joined = attach_tool_results(&outs, &ins);
        assert!(joined[0][0].1.is_none());
    }
}
```

- [ ] **Step 22.2: Expose the module**

In `server/ts-turn/src/lib.rs`, add:

```rust
pub mod tool_result_join;
```

- [ ] **Step 22.3: Run tests**

```
cargo test -p ts-turn tool_result_join -- --nocapture
```

Expected: 3 passed.

---

### Task 23: Extend `CallDetail` with `parsed` field

**Files:**
- Modify: `server/ts-storage/src/query.rs`
- Modify: `server/ts-storage/src/duckdb.rs`
- Modify: `server/ts-storage/src/sink.rs`

- [ ] **Step 23.1: Add successor-body fields to `CallDetail`**

In `query.rs`, extend `CallDetail`:

```rust
    /// Request body of the immediate successor call in the same turn, if any.
    /// Used by Phase-3 tool-result join in the API layer.
    #[serde(skip)]
    pub next_call_request_body: Option<String>,
```

- [ ] **Step 23.2: Update `query_call_by_id` SQL**

Modify `server/ts-storage/src/duckdb.rs`'s `query_call_by_id` (~line 1786) so it joins the call row against the successor call in the same `agent_turns` row.

Replacement SQL:

```sql
WITH me AS (
  SELECT c.*,
         t.turn_id AS my_turn_id,
         json_extract_string(t.call_ids, '$[*]') AS turn_call_ids_csv
    FROM llm_calls c
    LEFT JOIN agent_turns t
      ON json_array_contains(t.call_ids, c.id)
   WHERE c.id = ?
),
next_id AS (
  SELECT
    CASE WHEN (position(id || ',' in turn_call_ids_csv || ',') > 0)
         THEN substring(
                turn_call_ids_csv,
                position(id || ',' in turn_call_ids_csv || ',') + length(id) + 1,
                36
              )
         ELSE NULL
    END AS nid
  FROM me
),
next_body AS (
  SELECT c2.request_body AS next_body
    FROM next_id n
    LEFT JOIN llm_calls c2 ON c2.id = n.nid
)
SELECT m.*, nb.next_body FROM me m, next_body nb;
```

Notes:
- DuckDB's `json_extract_string(..., '$[*]')` returns a comma-separated list; `position(id || ','...)` locates the current call's index; advancing past it plus a length-36 UUID extracts the next one.
- If the current call is last (or only), `nid` is NULL and `next_body` is NULL.

If this SQL proves fragile, an acceptable fallback is: keep the current `query_call_by_id` SQL unchanged, add a **second** storage method `query_next_call_body(turn_id, current_id) -> Result<Option<String>>` with a plain lookup on `agent_turns.call_ids`. Either approach is fine — prefer the single-query version for cold-path correctness.

Update the `query_row` closure to read the new column into `next_call_request_body`.

- [ ] **Step 23.3: Update sink test double**

If `sink.rs` constructs `CallDetail`, add `next_call_request_body: None,` to each.

- [ ] **Step 23.4: Build + tests**

```
cargo test -p ts-storage query_call_by_id
```

Expected: existing test (`test_query_call_by_id` at ~line 3354) still passes. The new field is `None` in the existing fixture since there's no successor call.

---

### Task 24: Enrich `/api/llm-calls/:id` with `parsed` + joined results

**Files:**
- Modify: `server/ts-api/src/routes/llm_calls.rs`
- Modify: `server/ts-api/src/routes/turn_call_enrichment.rs` — add `enrich_single` for detail shape.

- [ ] **Step 24.1: Add `enrich_single` function**

In `turn_call_enrichment.rs`:

```rust
use ts_storage::query::CallDetail;

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedCallDetail {
    // flatten existing CallDetail fields — spread via Serialize through a manual struct
    #[serde(flatten)]
    pub base: CallDetail,
    pub parsed: ParsedCallContent,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParsedCallContent {
    pub reasoning: Option<String>,
    pub message: Option<String>,
    pub tool_calls: Vec<EnrichedToolCallFull>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedToolCallFull {
    pub id: String,
    pub name: String,
    pub args_json: String,
    pub result: Option<ToolResultFull>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolResultFull {
    pub content: String,
    pub size_bytes: u64,
    pub kind: &'static str,
    pub is_error: bool,
}

pub fn enrich_single(detail: CallDetail, registry: &WireApiRegistry) -> EnrichedCallDetail {
    let wire = registry.find_by_name(&detail.wire_api);
    let (parsed_out, _) = wire
        .map(|w| parse_bodies(w, detail.request_body.as_deref(), detail.response_body.as_deref()))
        .unwrap_or_default();
    let next_in = wire
        .map(|w| w.parse_input(&serde_json::from_str(detail.next_call_request_body.as_deref().unwrap_or("null")).unwrap_or(serde_json::Value::Null)))
        .unwrap_or_default();

    let tool_calls = parsed_out.tool_calls.into_iter().map(|tc| {
        let result = next_in.tool_results.iter().find(|tr| tr.tool_use_id == tc.id).map(|tr| {
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
    }).collect();

    EnrichedCallDetail {
        base: detail,
        parsed: ParsedCallContent {
            reasoning: parsed_out.reasoning,
            message: parsed_out.message,
            tool_calls,
        },
    }
}
```

- [ ] **Step 24.2: Wire into route**

In `server/ts-api/src/routes/llm_calls.rs` `detail()`:

```rust
use super::turn_call_enrichment::enrich_single;
use ts_llm::wire_apis::build_default_wire_api_registry;

pub async fn detail(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match storage.query_call_by_id(&id).await? {
        Some(detail) => {
            let registry = build_default_wire_api_registry();
            Ok(ApiResponse::ok(enrich_single(detail, &registry)))
        }
        None => Err(ApiError::NotFound(format!("call not found: {id}"))),
    }
}
```

- [ ] **Step 24.3: Manual check**

```
cargo build -p ts-api
just dev server
```

`curl http://localhost:8080/api/llm-calls/<known-call-id> | jq '.data.parsed'`. Expected: `parsed.tool_calls[].result` is either populated (if a next call exists and returned a result) or `null` (last call).

---

### Task 25: Frontend — expanded call card sections

**Files:**
- Modify: `console/src/types/api.ts` — extend `LlmCallDetail` with `parsed`.
- Modify: `console/src/components/turn-detail/call-card.tsx`.

- [ ] **Step 25.1: Extend TS types**

In `api.ts`:

```ts
export interface ToolResultFull {
  content: string
  size_bytes: number
  kind: "text" | "json" | "error" | "binary" | "missing"
  is_error: boolean
}

export interface EnrichedToolCallFull {
  id: string
  name: string
  args_json: string
  result: ToolResultFull | null
}

export interface ParsedCallContent {
  reasoning: string | null
  message: string | null
  tool_calls: EnrichedToolCallFull[]
}

export interface LlmCallDetail {
  // ...existing fields unchanged
  parsed: ParsedCallContent
}
```

- [ ] **Step 25.2: Expanded call card content**

`CallCard` needs to fetch `LlmCallDetail` when expanded. Introduce a minimal hook use inside the card (or lift detail fetching up):

```tsx
import { useLlmCallDetail } from "@/hooks/use-llm-call-detail"

// Inside CallCard, add when expanded:
const { data: detail } = useLlmCallDetail(expanded ? call.id : null)
```

Then render the three subsections in the expanded area (above the meta row):

```tsx
{expanded && (
  <div className="border-t border-border px-3 py-2 space-y-3 text-xs">
    {detail?.parsed.reasoning && (
      <details className="rounded border border-border/50 p-2" open={false}>
        <summary className="cursor-pointer text-muted-foreground">Reasoning</summary>
        <pre className="mt-2 max-h-[600px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">{detail.parsed.reasoning}</pre>
      </details>
    )}
    {detail?.parsed.message && (
      <details className="rounded border border-border/50 p-2" open>
        <summary className="cursor-pointer text-muted-foreground">Message</summary>
        <div className="mt-2 max-h-[400px] overflow-auto text-[11px]">
          <Markdown text={detail.parsed.message} />
        </div>
      </details>
    )}
    {detail?.parsed.tool_calls && detail.parsed.tool_calls.length > 0 && (
      <div className="rounded border border-border/50 p-2">
        <div className="mb-1 text-muted-foreground">Tool calls ({detail.parsed.tool_calls.length})</div>
        <div className="space-y-2">
          {detail.parsed.tool_calls.map((tc) => (
            <ToolCallRow key={tc.id} tc={tc} />
          ))}
        </div>
      </div>
    )}
    <div className="text-muted-foreground">
      {call.wire_api} · TTFB {formatMs(call.ttfb_ms)} · finish: {call.finish_reason ?? "—"}
    </div>
    <button onClick={() => onOpenRawHttp?.(call.id)} className="text-foreground hover:underline">View raw HTTP →</button>
  </div>
)}
```

- [ ] **Step 25.3: Tool call row + result summary**

```tsx
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

function formatArgs(s: string): string {
  try { return JSON.stringify(JSON.parse(s), null, 2) } catch { return s }
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}
```

- [ ] **Step 25.4: Build + dev-server check**

```
cd console && bun run build
```

Manual: expand a call that emitted tool_use. Expected: Reasoning subsection visible only if the model emitted reasoning; Message subsection shows the LLM's text; Tool calls subsection lists each tool with expandable args JSON and a result summary that expands into full content. Last call's tool_uses show "(no response, turn ended)".

---

### Task 26: Phase 3 polish and commit

- [ ] **Step 26.1: Run both quality gates**

```
just quality rs
just quality ts
```

- [ ] **Step 26.2: Full workspace test**

```
cargo test --workspace
```

- [ ] **Step 26.3: Manual full-flow regression**

Against a running `just dev server` + `just dev console`:
- Open a 20-call turn → Gantt navigable, cards show tool chips + previews
- Expand mid-turn tool-call → see Reasoning (if present), Message, Tool calls with args + result summary
- Expand last call that still emitted tools → result shows "(no response, turn ended)"
- "View raw HTTP →" still opens drawer with headers/bodies
- Deep-link URL still works

- [ ] **Step 26.4: Commit Phase 3**

```bash
git add server/ts-turn/src/lib.rs \
        server/ts-turn/src/tool_result_join.rs \
        server/ts-storage/src/query.rs \
        server/ts-storage/src/duckdb.rs \
        server/ts-storage/src/sink.rs \
        server/ts-api/src/routes/llm_calls.rs \
        server/ts-api/src/routes/turn_call_enrichment.rs \
        console/src/types/api.ts \
        console/src/components/turn-detail/call-card.tsx
git commit -m "$(cat <<'EOF'
feat(turn-detail): join tool_use to tool_result + enrich /llm-calls detail

Phase 3 of turn-detail redesign. Adds attach_tool_results in ts-turn to
pair tool_use blocks from call N with tool_results from call N+1.
/api/llm-calls/:id now returns parsed.reasoning, parsed.message, and
parsed.tool_calls[] with full args and joined result. Expanded call cards
render these subsections with expand/collapse for args and result content.
Orphan tool_uses in the final call render "(no response, turn ended)".
EOF
)"
```

---

## Self-Review Checklist

After the final task:

- [ ] Every spec section covered: layout ✓, top bar ✓, stats cards ✓, gantt nav ✓, user + final cards ✓, call card collapsed + expanded ✓, raw HTTP drawer ✓, URL sync ✓, loading/error/empty ✓, keyboard ✓, backend parsers ✓, storage extensions ✓, API payload extensions ✓, cross-call result join ✓.
- [ ] No placeholder text (`TBD`, `TODO`, "implement later").
- [ ] Commits match user directive: exactly 3 commits (one per phase).
- [ ] Types consistent across tasks: `EnrichedTurnCallItem` (list shape), `EnrichedCallDetail` (detail shape), `ParsedOutput` / `ParsedInput` (parser outputs), `ParsedToolCall` / `ParsedToolResult` (parser inner types).
