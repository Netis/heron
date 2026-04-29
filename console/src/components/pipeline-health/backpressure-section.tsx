import type { CSSProperties } from "react"
import { cn } from "@/lib/utils"
import { WARNING_THRESHOLD, CRITICAL_THRESHOLD } from "@/lib/pipeline-health"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

type Health = "ok" | "warn" | "bad"

function classifyQueue(value: number, capacity: number): Health {
  if (capacity <= 0) return "ok"
  const r = value / capacity
  if (r >= CRITICAL_THRESHOLD) return "bad"
  if (r >= WARNING_THRESHOLD) return "warn"
  return "ok"
}

const CELL_STYLE: Record<Health, string> = {
  ok: "bg-card border-border",
  warn: "bg-amber-50 border-amber-300 dark:bg-amber-950/40 dark:border-amber-600",
  bad: "bg-red-50 border-red-300 dark:bg-red-950/40 dark:border-red-600",
}

const BAR_STYLE: Record<Health, string> = {
  ok: "bg-emerald-500",
  warn: "bg-amber-500",
  bad: "bg-red-500",
}

// ---------------------------------------------------------------------------
// Grid topology
// ---------------------------------------------------------------------------
// 13 columns alternating stage (60px) / queue (140px), with storage (80px)
// pinned to col 13. Storage spans all 4 rows so the fan-in reads as a single
// visual entity. Rows 2-4 are branches; their queues sit directly under the
// producer stage on row 1, and the trailing dashed connector lands on storage.
//
// Col index map:
//   1  cap         (stage)
//   2  q_raw_pkts
//   3  disp        (stage)
//   4  q_parsed_pkts
//   5  proto       (stage)
//   6  q_http_parse_events
//   7  joiner      (stage)            row 2 fork-indicator sits here
//   8  q_http_joiner_events           row 2: q_exchanges
//   9  llm         (stage)            rows 3-4 fork-indicator sits here
//   10 q_agent_calls                  row 3: q_calls / row 4: q_llm_events
//   11 turn        (stage)            row 4: metrics (stage)
//   12 q_turns                        row 4: q_metrics
//   13 storage     (row-span 1..4)
const GRID_TEMPLATE_COLS =
  "60px 140px 60px 140px 60px 140px 60px 140px 60px 140px 60px 140px 80px"

function area(row: number | string, col: number | string): CSSProperties {
  return { gridRow: row, gridColumn: col }
}

// ---------------------------------------------------------------------------
// Cell components
// ---------------------------------------------------------------------------

function StagePill({
  row,
  col,
  label,
}: {
  row: number
  col: number
  label: string
}) {
  return (
    <div
      className="flex items-center justify-center rounded-md border border-border bg-muted px-1 py-1 text-center text-[10px] font-semibold uppercase tracking-wider text-muted-foreground"
      style={area(row, col)}
    >
      {label}
    </div>
  )
}

function QueueCell({
  row,
  col,
  name,
  value,
  capacity,
}: {
  row: number
  col: number
  name: string
  value: number
  capacity: number
}) {
  const health = classifyQueue(value, capacity)
  const pct = capacity > 0 ? Math.round((value / capacity) * 100) : 0
  const filled = capacity > 0 ? Math.min(100, (value / capacity) * 100) : 0
  return (
    <div
      className={cn("rounded-md border p-1.5", CELL_STYLE[health])}
      style={area(row, col)}
    >
      <div className="truncate text-[10px] font-semibold text-foreground">
        {name}
      </div>
      <div className="mt-1 h-1 overflow-hidden rounded-full bg-muted">
        <div
          className={cn("h-full", BAR_STYLE[health])}
          style={{ width: `${filled}%` }}
        />
      </div>
      <div className="mt-1 text-[10px] tabular-nums text-muted-foreground">
        {value.toLocaleString()}/{capacity.toLocaleString()} ({pct}%)
      </div>
    </div>
  )
}

function ForkIndicator({ row, col }: { row: number; col: number }) {
  // Sits in the column under the producer stage, on the branch row, to make
  // "this branch comes from the stage above" visually unambiguous.
  return (
    <div
      className="flex items-center justify-center text-base text-muted-foreground/70 select-none"
      style={area(row, col)}
      aria-hidden
    >
      ↳
    </div>
  )
}

function Connector({
  row,
  colStart,
  colEnd,
}: {
  row: number
  colStart: number
  colEnd: number
}) {
  // Dashed horizontal line spanning empty grid columns between a branch's
  // last queue and the shared storage cell.
  return (
    <div
      className="flex h-full items-center"
      style={{ gridRow: row, gridColumn: `${colStart} / ${colEnd}` }}
    >
      <div className="h-0 w-full border-t border-dashed border-muted-foreground/40" />
    </div>
  )
}

function StorageColumn() {
  return (
    <div
      className="flex items-center justify-center rounded-md border border-border bg-secondary text-[11px] font-bold uppercase tracking-wider text-muted-foreground"
      style={{ gridRow: "1 / span 4", gridColumn: 13 }}
    >
      storage
    </div>
  )
}

// ---------------------------------------------------------------------------
// Section
// ---------------------------------------------------------------------------

export function BackpressureSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]
  const byName = new Map(all.map((m) => [m.name, m]))

  // Pull a queue snapshot, defaulting to (0, 0). Capacity == 0 disables the
  // health classification (renders as ok with empty bar) — happens when a
  // queue metric isn't reported (e.g., before the first probe sample).
  const q = (name: string) => {
    const m = byName.get(name)
    if (m && m.kind === "gauge") {
      return { value: m.value, capacity: m.capacity ?? 0 }
    }
    return { value: 0, capacity: 0 }
  }

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ① Backpressure
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Pipeline laid out left-to-right. Joiner and LLM are fan-out stages —
        each branch starts on a new row directly beneath its producer; the
        dashed lines lead to the shared <strong>storage</strong> sink on the
        right (fan-in). The first queue cell to redden is the bottleneck.
      </p>

      <div className="overflow-x-auto">
        <div
          className="grid items-stretch gap-x-2 gap-y-3"
          style={{
            gridTemplateColumns: GRID_TEMPLATE_COLS,
            minWidth: "1320px",
          }}
        >
          {/* ===== Row 1 — main spine ============================================ */}
          <StagePill row={1} col={1} label="cap" />
          <QueueCell row={1} col={2} name="q_raw_pkts" {...q("q_raw_pkts")} />
          <StagePill row={1} col={3} label="disp" />
          <QueueCell
            row={1}
            col={4}
            name="q_parsed_pkts"
            {...q("q_parsed_pkts")}
          />
          <StagePill row={1} col={5} label="proto" />
          <QueueCell
            row={1}
            col={6}
            name="q_http_parse_events"
            {...q("q_http_parse_events")}
          />
          <StagePill row={1} col={7} label="joiner" />
          <QueueCell
            row={1}
            col={8}
            name="q_http_joiner_events"
            {...q("q_http_joiner_events")}
          />
          <StagePill row={1} col={9} label="llm" />
          <QueueCell
            row={1}
            col={10}
            name="q_agent_calls"
            {...q("q_agent_calls")}
          />
          <StagePill row={1} col={11} label="turn" />
          <QueueCell row={1} col={12} name="q_turns" {...q("q_turns")} />

          {/* ===== Row 2 — joiner → q_exchanges → storage ======================== */}
          <ForkIndicator row={2} col={7} />
          <QueueCell row={2} col={8} name="q_exchanges" {...q("q_exchanges")} />
          <Connector row={2} colStart={9} colEnd={13} />

          {/* ===== Row 3 — llm → q_calls → storage =============================== */}
          <ForkIndicator row={3} col={9} />
          <QueueCell row={3} col={10} name="q_calls" {...q("q_calls")} />
          <Connector row={3} colStart={11} colEnd={13} />

          {/* ===== Row 4 — llm → q_llm_events → metrics → q_metrics → storage ==== */}
          <ForkIndicator row={4} col={9} />
          <QueueCell
            row={4}
            col={10}
            name="q_llm_events"
            {...q("q_llm_events")}
          />
          <StagePill row={4} col={11} label="metrics" />
          <QueueCell row={4} col={12} name="q_metrics" {...q("q_metrics")} />

          {/* ===== Storage column — fan-in ======================================= */}
          <StorageColumn />
        </div>
      </div>
    </section>
  )
}
