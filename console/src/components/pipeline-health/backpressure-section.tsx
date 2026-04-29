import { cn } from "@/lib/utils"
import { WARNING_THRESHOLD, CRITICAL_THRESHOLD } from "@/lib/pipeline-health"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

// Display order — left to right along the data flow.
const QUEUE_ORDER = [
  "q_raw_pkts",
  "q_parsed_pkts",
  "q_http_parse_events",
  "q_http_joiner_events",
  "q_agent_calls",
  "q_llm_events",
] as const

const STORAGE_QUEUES = ["q_calls", "q_turns", "q_metrics", "q_exchanges"] as const

function classifyQueue(value: number, capacity: number) {
  if (capacity <= 0) return "ok" as const
  const r = value / capacity
  if (r >= CRITICAL_THRESHOLD) return "bad" as const
  if (r >= WARNING_THRESHOLD) return "warn" as const
  return "ok" as const
}

const STAGE_STYLES = {
  ok: "bg-card border-border",
  warn: "bg-amber-50 border-amber-300 dark:bg-amber-950/40 dark:border-amber-600",
  bad: "bg-red-50 border-red-300 dark:bg-red-950/40 dark:border-red-600",
} as const

const BAR_STYLES = {
  ok: "bg-emerald-500",
  warn: "bg-amber-500",
  bad: "bg-red-500",
} as const

function QueueCell({
  name,
  value,
  capacity,
}: {
  name: string
  value: number
  capacity: number
}) {
  const cls = classifyQueue(value, capacity)
  const pct = capacity > 0 ? Math.round((value / capacity) * 100) : 0
  return (
    <div
      className={cn(
        "min-w-[140px] rounded-md border p-2",
        STAGE_STYLES[cls],
      )}
    >
      <div className="text-xs font-semibold text-foreground">{name}</div>
      <div className="text-xs tabular-nums text-muted-foreground">
        {value.toLocaleString()} / {capacity.toLocaleString()} ({pct}%)
      </div>
      <div className="mt-1 h-1 overflow-hidden rounded-full bg-muted">
        <div
          className={cn("h-full", BAR_STYLES[cls])}
          style={{ width: `${Math.min(100, pct)}%` }}
        />
      </div>
    </div>
  )
}

export function BackpressureSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]
  const byName = new Map(all.map((m) => [m.name, m]))

  const cells: Array<{ name: string; value: number; capacity: number }> = []
  for (const name of QUEUE_ORDER) {
    const m = byName.get(name)
    if (m && m.kind === "gauge" && m.capacity) {
      cells.push({ name: m.name, value: m.value, capacity: m.capacity })
    }
  }

  // Storage queues: aggregate to one summary cell ("worst-of"), keep individual
  // queues visible in the all-metrics table (Section ⑤).
  const storageCells = STORAGE_QUEUES.map((n) => byName.get(n)).filter(
    (m): m is MetricRecord =>
      !!m && m.kind === "gauge" && typeof m.capacity === "number",
  )
  let storageSummary: { value: number; capacity: number } | null = null
  if (storageCells.length > 0) {
    let worstRatio = -1
    for (const m of storageCells) {
      const r = m.value / (m.capacity ?? 1)
      if (r > worstRatio) {
        worstRatio = r
        storageSummary = { value: m.value, capacity: m.capacity ?? 0 }
      }
    }
  }

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ① Backpressure
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Queue depth at every stage along the pipeline. The first cell to redden
        is the bottleneck.
      </p>
      <div className="flex items-stretch gap-1 overflow-x-auto">
        {cells.map((c, i) => (
          <div key={c.name} className="flex items-stretch gap-1">
            <QueueCell {...c} />
            {(i < cells.length - 1 || storageSummary) && (
              <div className="self-center px-1 text-muted-foreground">→</div>
            )}
          </div>
        ))}
        {storageSummary && (
          <QueueCell
            name="storage queues (worst)"
            value={storageSummary.value}
            capacity={storageSummary.capacity}
          />
        )}
      </div>
    </section>
  )
}
