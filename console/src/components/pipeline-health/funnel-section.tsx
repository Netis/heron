import { computeFunnel } from "@/lib/pipeline-health"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

export function FunnelSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]
  const rows = computeFunnel(all)

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ② Throughput Funnel
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Records flowing from packet capture to storage flush. Each bar's length
        is proportional to the count; drops/fan-ins are noted directly under
        each stage.
      </p>
      <div className="flex flex-col gap-1.5">
        {rows.map((row) => (
          <div key={row.label} className="grid grid-cols-[180px_1fr_120px] items-center gap-2">
            <div className="text-xs font-semibold text-foreground">
              {row.label}
            </div>
            <div className="h-3.5 rounded bg-muted">
              <div
                className="h-full rounded bg-blue-500/80"
                style={{
                  width:
                    row.widthRatio === 0
                      ? "0%"
                      : `${Math.max(2, row.widthRatio * 100)}%`,
                }}
              />
            </div>
            <div className="text-right text-xs font-semibold tabular-nums text-foreground">
              {row.value.toLocaleString()}
            </div>
            {row.dropAnnotation && (
              <div className="col-start-2 col-end-3 text-[10px] text-amber-700 dark:text-amber-400">
                ↳ {row.dropAnnotation}
              </div>
            )}
          </div>
        ))}
      </div>
    </section>
  )
}
