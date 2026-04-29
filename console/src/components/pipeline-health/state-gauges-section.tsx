import { cn } from "@/lib/utils"
import type { MetricRecord, MetricGroup } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

// Pipeline-stage order — gauges are bucketed by this so each row reads
// like a step in the data flow.
const GROUP_ORDER: MetricGroup[] = [
  "capture",
  "protocol",
  "llm",
  "turn",
  "metrics",
  "storage",
]

function StateCard({ label, value }: { label: string; value: number }) {
  return (
    <div
      className={cn(
        "flex min-w-[120px] flex-col gap-0.5 rounded-md border border-border bg-muted/30 px-2.5 py-1.5",
      )}
    >
      <span className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      <div className="text-base font-bold tabular-nums text-foreground">
        {value.toLocaleString()}
      </div>
    </div>
  )
}

export function StateGaugesSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]

  // Pick every gauge except queue depths (which are owned by the Backpressure
  // section). Filtering by kind/prefix — instead of a hardcoded list — means
  // any new gauge added in `ts-common/internal_metrics.rs` shows up here
  // automatically. That matters for OOM diagnosis: an unbounded gauge that
  // silently grows without being on this page is the worst case.
  const gauges = all.filter(
    (m) => m.kind === "gauge" && !m.name.startsWith("q_"),
  )

  // Bucket by group; sort within each group alphabetically for stable layout.
  const byGroup = new Map<MetricGroup, MetricRecord[]>()
  for (const m of gauges) {
    const arr = byGroup.get(m.group) ?? []
    arr.push(m)
    byGroup.set(m.group, arr)
  }
  for (const arr of byGroup.values()) {
    arr.sort((a, b) => a.name.localeCompare(b.name))
  }

  const groupRows = GROUP_ORDER.flatMap((group) => {
    const items = byGroup.get(group)
    if (!items || items.length === 0) return []
    return [{ group, items }]
  })

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ③ Live Gauges
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Uncapped gauges, grouped by pipeline stage. Watch for unbounded growth —
        these are the leading indicators of OOM. Capped queue gauges live in
        the Backpressure section above.
      </p>
      {groupRows.length === 0 ? (
        <div className="text-xs text-muted-foreground">No gauges reported.</div>
      ) : (
        <div className="flex flex-col gap-2">
          {groupRows.map(({ group, items }) => (
            <div
              key={group}
              className="flex items-start gap-3 border-t border-border/60 pt-2 first:border-t-0 first:pt-0"
            >
              <div className="w-16 shrink-0 pt-1.5 text-[10px] font-bold uppercase tracking-wider text-muted-foreground">
                {group}
              </div>
              <div className="flex flex-1 flex-wrap gap-2">
                {items.map((m) => (
                  <StateCard key={m.name} label={m.name} value={m.value} />
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </section>
  )
}
