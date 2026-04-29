import { cn } from "@/lib/utils"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

const STATE_METRICS = [
  "flows_active",
  "turns_active",
  "tcp_ooo_buffered",
  "flows_expired",
  "heartbeats_emitted",
  "batches_received",
  "http_resyncs",
] as const

function StateCard({ label, value }: { label: string; value: number | undefined }) {
  return (
    <div
      className={cn(
        "flex flex-col gap-0.5 rounded-md border border-border bg-muted/30 p-2.5",
      )}
    >
      <div className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="text-base font-bold tabular-nums text-foreground">
        {value === undefined ? "—" : value.toLocaleString()}
      </div>
    </div>
  )
}

export function StateGaugesSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]
  const byName = new Map(all.map((m) => [m.name, m]))
  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ③ State Gauges
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Instance-level state — counts not arranged along the pipeline.
      </p>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 md:grid-cols-5 lg:grid-cols-7">
        {STATE_METRICS.map((name) => (
          <StateCard key={name} label={name} value={byName.get(name)?.value} />
        ))}
      </div>
    </section>
  )
}
