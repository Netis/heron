import { computeFunnel, type FunnelStageName } from "@/lib/pipeline-health"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

// Stage badge color — same hue family as group order so the funnel reads
// "stage A → stage B" at a glance without spelling it out on every row.
const STAGE_BADGE: Record<FunnelStageName, string> = {
  capture: "bg-sky-100 text-sky-800 dark:bg-sky-900/40 dark:text-sky-200",
  dispatcher: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-200",
  net: "bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-200",
  http: "bg-teal-100 text-teal-800 dark:bg-teal-900/40 dark:text-teal-200",
  joiner:
    "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-200",
  llm: "bg-violet-100 text-violet-800 dark:bg-violet-900/40 dark:text-violet-200",
  turn: "bg-fuchsia-100 text-fuchsia-800 dark:bg-fuchsia-900/40 dark:text-fuchsia-200",
  metrics: "bg-pink-100 text-pink-800 dark:bg-pink-900/40 dark:text-pink-200",
  storage: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-200",
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
        Records flowing through every pipeline stage. Each row shows{" "}
        <strong>count → survival ratio vs its direct upstream</strong>; the
        upstream is named on every row so the basis is explicit. Rows tagged{" "}
        <em>filter</em> deliberately reduce volume — a low ratio there is{" "}
        <em>not</em> data loss. Rows without a comparable upstream show only
        the count.
      </p>
      <div className="flex flex-col gap-1">
        {rows.map((row, i) => {
          const stageChanged = i === 0 || rows[i - 1].stage !== row.stage
          const ratio = row.widthRatio ?? 0
          const showPct = row.kind === "normal" || row.kind === "filter"
          const pctText = showPct ? `${Math.round(ratio * 100)}%` : ""

          // Caption goes in the right-most cell. Only rows with a percentage
          // (`normal` / `filter`) get a caption — root/info/fanIn leave it
          // empty so the numeric columns line up without decorative tags.
          let caption: React.ReactNode = null
          if (row.kind === "normal") {
            caption = <span>of {row.upstream}</span>
          } else if (row.kind === "filter") {
            caption = (
              <span>
                of {row.upstream}
                <span className="ml-1 text-muted-foreground/70">· filter</span>
              </span>
            )
          }

          return (
            <div
              key={row.label}
              className="grid grid-cols-[80px_220px_80px_60px_1fr] items-baseline gap-x-3"
            >
              <div>
                {stageChanged && (
                  <span
                    className={`inline-block rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wider ${STAGE_BADGE[row.stage]}`}
                  >
                    {row.stage}
                  </span>
                )}
              </div>
              <div className="text-xs font-semibold text-foreground">
                {row.label}
              </div>
              <div className="text-right text-xs font-semibold tabular-nums text-foreground">
                {row.value.toLocaleString()}
              </div>
              <div className="text-right text-[11px] tabular-nums text-muted-foreground">
                {pctText}
              </div>
              <div className="text-[11px] italic text-muted-foreground">
                {caption}
              </div>
              {row.dropAnnotation && (
                <div className="col-start-3 col-end-6 text-[10px] text-amber-700 dark:text-amber-400">
                  ↳ {row.dropAnnotation}
                </div>
              )}
            </div>
          )
        })}
      </div>
    </section>
  )
}
