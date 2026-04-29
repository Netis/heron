import { useMemo, useState } from "react"
import {
  CRITICAL_DELTA_COUNTERS,
  CRITICAL_THRESHOLD,
  WARNING_DELTA_COUNTERS,
  WARNING_THRESHOLD,
} from "@/lib/pipeline-health"
import { cn } from "@/lib/utils"
import { usePipelineHealthStore } from "@/stores/pipeline-health"
import type { MetricGroup, MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
  prevByName: Record<string, number>
  ts: number
  prevTs: number | null
}

const GROUPS: Array<MetricGroup | "all"> = [
  "all",
  "capture",
  "protocol",
  "llm",
  "turn",
  "metrics",
  "storage",
]

type SortKey = "group" | "name" | "value" | "delta" | "cap"
type SortDir = "asc" | "desc"

export function AllMetricsTable({
  pipelineMetrics,
  globalMetrics,
  prevByName,
  ts,
  prevTs,
}: Props) {
  const groupFilter = usePipelineHealthStore((s) => s.tableGroupFilter)
  const onlyWarn = usePipelineHealthStore((s) => s.tableOnlyWarn)
  const setGroupFilter = usePipelineHealthStore((s) => s.setTableGroupFilter)
  const setOnlyWarn = usePipelineHealthStore((s) => s.setTableOnlyWarn)

  const [sortKey, setSortKey] = useState<SortKey>("group")
  const [sortDir, setSortDir] = useState<SortDir>("asc")

  const dt = prevTs && ts > prevTs ? ts - prevTs : 0

  const rows = useMemo(() => {
    const all = [...pipelineMetrics, ...globalMetrics]
    return all.map((m) => {
      const prev = prevByName[m.name]
      const delta = m.kind === "counter" && typeof prev === "number" ? m.value - prev : null
      const ratio =
        m.kind === "gauge" && m.capacity && m.capacity > 0 ? m.value / m.capacity : null
      const warnLevel: "critical" | "warning" | null =
        (CRITICAL_DELTA_COUNTERS.has(m.name) && (delta ?? 0) > 0) ||
        (ratio !== null && ratio >= CRITICAL_THRESHOLD)
          ? "critical"
          : (WARNING_DELTA_COUNTERS.has(m.name) &&
                ((delta ?? 0) > 0 || m.value > 0)) ||
              (ratio !== null && ratio >= WARNING_THRESHOLD)
            ? "warning"
            : null
      return { m, delta, ratio, warnLevel }
    })
  }, [pipelineMetrics, globalMetrics, prevByName])

  const filtered = rows.filter(
    (r) =>
      (groupFilter === "all" || r.m.group === groupFilter) &&
      (!onlyWarn || r.warnLevel !== null),
  )

  const sorted = [...filtered].sort((a, b) => {
    const dir = sortDir === "asc" ? 1 : -1
    switch (sortKey) {
      case "group":
        return dir * (a.m.group.localeCompare(b.m.group) || a.m.name.localeCompare(b.m.name))
      case "name":
        return dir * a.m.name.localeCompare(b.m.name)
      case "value":
        return dir * (a.m.value - b.m.value)
      case "delta":
        return dir * ((a.delta ?? 0) - (b.delta ?? 0))
      case "cap":
        return dir * ((a.ratio ?? -1) - (b.ratio ?? -1))
    }
  })

  const sortBtn = (key: SortKey, label: string) => (
    <button
      onClick={() => {
        if (sortKey === key) setSortDir(sortDir === "asc" ? "desc" : "asc")
        else {
          setSortKey(key)
          setSortDir("asc")
        }
      }}
      className="text-left font-semibold"
    >
      {label}
      {sortKey === key && (sortDir === "asc" ? " ↑" : " ↓")}
    </button>
  )

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ⑤ All Metrics
      </h3>
      <details>
        <summary className="cursor-pointer text-xs font-medium text-blue-600 hover:underline dark:text-blue-400">
          Show all {pipelineMetrics.length + globalMetrics.length} metrics
        </summary>

        <div className="mt-3 flex flex-wrap items-center gap-1.5">
          {GROUPS.map((g) => (
            <button
              key={g}
              onClick={() => setGroupFilter(g)}
              className={cn(
                "rounded-full border px-2.5 py-0.5 text-xs",
                groupFilter === g
                  ? "border-foreground bg-foreground text-background"
                  : "border-border bg-card text-muted-foreground hover:bg-muted",
              )}
            >
              {g}
            </button>
          ))}
          <button
            onClick={() => setOnlyWarn(!onlyWarn)}
            className={cn(
              "ml-auto rounded-full border px-2.5 py-0.5 text-xs",
              onlyWarn
                ? "border-amber-400 bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300"
                : "border-border bg-card text-muted-foreground hover:bg-muted",
            )}
          >
            ⚠ only
          </button>
        </div>

        <table className="mt-2 w-full text-xs">
          <thead>
            <tr className="border-b border-border">
              <th className="px-2 py-1 text-left">{sortBtn("group", "group")}</th>
              <th className="px-2 py-1 text-left">{sortBtn("name", "metric")}</th>
              <th className="px-2 py-1 text-left">kind</th>
              <th className="px-2 py-1 text-right">{sortBtn("value", "value")}</th>
              <th className="px-2 py-1 text-right">{sortBtn("delta", "Δ/s")}</th>
              <th className="px-2 py-1 text-right">{sortBtn("cap", "cap%")}</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map(({ m, delta, ratio, warnLevel }) => (
              <tr
                key={m.name}
                className={cn(
                  "border-b border-border/60",
                  warnLevel === "critical" &&
                    "bg-red-50 dark:bg-red-950/40",
                  warnLevel === "warning" &&
                    "bg-amber-50 dark:bg-amber-950/40",
                )}
              >
                <td className="px-2 py-0.5">{m.group}</td>
                <td className="px-2 py-0.5 font-mono">{m.name}</td>
                <td className="px-2 py-0.5">{m.kind}</td>
                <td className="px-2 py-0.5 text-right tabular-nums">
                  {m.value.toLocaleString()}
                </td>
                <td className="px-2 py-0.5 text-right tabular-nums">
                  {m.kind === "counter" && delta !== null && dt > 0
                    ? `${delta >= 0 ? "+" : ""}${(delta / dt).toFixed(1)}`
                    : "—"}
                </td>
                <td className="px-2 py-0.5 text-right tabular-nums">
                  {ratio !== null ? `${Math.round(ratio * 100)}%` : "—"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </details>
    </section>
  )
}
