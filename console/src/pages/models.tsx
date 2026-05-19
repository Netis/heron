import { useState } from "react"
import { ArrowUpDown, ChevronRight } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { useModels, useTimeseries } from "@/hooks/use-metrics"
import { TimeseriesLineChart } from "@/components/charts/timeseries-line-chart"
import type { MetricsModelRow, TimeseriesData } from "@/types/api"

type SortKey =
  | "model"
  | "wire_api"
  | "call_count"
  | "error_rate"
  | "ttft_avg"
  | "ttft_p95"
  | "e2e_avg"
  | "e2e_p95"
  | "tpot_avg"
  | "total_input_tokens"
  | "total_output_tokens"
type SortOrder = "asc" | "desc"

function getErrorRate(m: MetricsModelRow): number {
  return m.call_count > 0 ? (m.error_count / m.call_count) * 100 : 0
}

function getSortValue(m: MetricsModelRow, key: SortKey): number | string {
  if (key === "error_rate") return getErrorRate(m)
  if (key === "model" || key === "wire_api") return m[key]
  // Underlying field is tpot_avg (ms/token, lower = faster) but the column
  // surfaces it as TPS = 1000/tpot_avg. Invert the sort value so "desc"
  // gives fastest first — matches what a user clicking "Generation TPS"
  // expects to see.
  if (key === "tpot_avg") {
    return m.tpot_avg != null && m.tpot_avg > 0 ? 1000 / m.tpot_avg : 0
  }
  return (m[key] as number) ?? 0
}

const LATENCY_SERIES = [
  { key: "ttft_avg", label: "TTFT avg", color: "#f59e0b" },
  { key: "ttft_p95", label: "TTFT p95", color: "#f59e0b", dash: "5 3" },
  { key: "e2e_avg", label: "E2E avg", color: "#3b82f6" },
  { key: "e2e_p95", label: "E2E p95", color: "#3b82f6", dash: "5 3" },
]

const VOLUME_SERIES = [
  { key: "call_count", label: "Calls", color: "#3b82f6" },
  { key: "error_count", label: "Errors", color: "#ef4444" },
]

/** Filter grouped timeseries data to a single model */
function filterByModel(data: TimeseriesData | undefined, model: string): TimeseriesData | null {
  if (!data) return null
  const filtered = data.series.filter((s) => s.group === model)
  if (filtered.length === 0) return null
  // Re-map: remove group, keep name as key
  return {
    timestamps: data.timestamps,
    series: filtered.map((s) => ({ ...s, group: null })),
  }
}

function ModelDetailCharts({ model }: { model: string }) {
  const { data: latencyData } = useTimeseries("ttft_avg,ttft_p95,e2e_avg,e2e_p95", {
    groupBy: "model",
  })
  const { data: volumeData } = useTimeseries("call_count,error_count", {
    groupBy: "model",
  })

  const modelLatency = filterByModel(latencyData, model)
  const modelVolume = filterByModel(volumeData, model)

  return (
    <div className="grid grid-cols-2 gap-4">
      <div className="rounded-lg border border-border bg-card p-4">
        <h3 className="mb-3 text-sm font-medium">
          Latency Over Time — <span className="text-muted-foreground">{model}</span>
        </h3>
        <TimeseriesLineChart data={modelLatency} series={LATENCY_SERIES} yFormatter={formatMs} />
      </div>
      <div className="rounded-lg border border-border bg-card p-4">
        <h3 className="mb-3 text-sm font-medium">
          Call Volume & Errors — <span className="text-muted-foreground">{model}</span>
        </h3>
        <TimeseriesLineChart
          data={modelVolume}
          series={VOLUME_SERIES}
          yFormatter={(v) => formatNumber(v)}
          variant="area"
        />
      </div>
    </div>
  )
}

export function ModelsPage() {
  const { data: modelsData } = useModels()
  const [sortKey, setSortKey] = useState<SortKey>("call_count")
  const [sortOrder, setSortOrder] = useState<SortOrder>("desc")
  const [selectedModel, setSelectedModel] = useState<string | null>(null)

  const models = modelsData?.models ?? []

  function handleSort(key: SortKey) {
    if (key === sortKey) {
      setSortOrder(sortOrder === "asc" ? "desc" : "asc")
    } else {
      setSortKey(key)
      setSortOrder("desc")
    }
  }

  const sorted = [...models].sort((a, b) => {
    const av = getSortValue(a, sortKey)
    const bv = getSortValue(b, sortKey)
    if (typeof av === "string" && typeof bv === "string") {
      return sortOrder === "asc" ? av.localeCompare(bv) : bv.localeCompare(av)
    }
    return sortOrder === "asc" ? (av as number) - (bv as number) : (bv as number) - (av as number)
  })

  function SortHeader({ label, field, align }: { label: string; field: SortKey; align?: "left" | "right" }) {
    const active = sortKey === field
    return (
      <button
        className={cn(
          "inline-flex items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground",
          align === "right" && "justify-end",
        )}
        onClick={() => handleSort(field)}
      >
        {label}
        <ArrowUpDown className={`size-3 ${active ? "text-foreground" : "opacity-40"}`} />
      </button>
    )
  }

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* Model Comparison Table */}
      <div className="rounded-lg border border-border bg-card">
        <div className="overflow-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-border">
                <th className="px-4 py-3 text-left"><SortHeader label="Model" field="model" align="left" /></th>
                <th className="px-3 py-3 text-left"><SortHeader label="Wire API" field="wire_api" align="left" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="Calls" field="call_count" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="Error %" field="error_rate" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="TTFT avg" field="ttft_avg" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="TTFT p95" field="ttft_p95" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="E2E avg" field="e2e_avg" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="E2E p95" field="e2e_p95" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="Generation TPS" field="tpot_avg" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="In Tokens" field="total_input_tokens" align="right" /></th>
                <th className="px-3 py-3 text-right"><SortHeader label="Out Tokens" field="total_output_tokens" align="right" /></th>
                <th className="w-8 px-2 py-3" />
              </tr>
            </thead>
            <tbody>
              {sorted.length === 0 ? (
                <tr>
                  <td colSpan={12} className="py-12 text-center text-muted-foreground">
                    No models found in selected time range
                  </td>
                </tr>
              ) : (
                sorted.map((m) => {
                  const errRate = getErrorRate(m)
                  const isSelected = selectedModel === m.model
                  return (
                    <tr
                      key={m.model}
                      className={cn(
                        "cursor-pointer border-b border-border/30 transition-colors hover:bg-muted/30",
                        isSelected && "bg-muted/50",
                      )}
                      onClick={() => setSelectedModel(isSelected ? null : m.model)}
                    >
                      <td className="px-4 py-2.5">
                        <span className="font-medium" title={m.model}>
                          {m.model}
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-muted-foreground">{m.wire_api}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{formatNumber(m.call_count)}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        <span
                          className={
                            errRate > 5
                              ? "text-red-500"
                              : errRate > 1
                                ? "text-amber-500"
                                : ""
                          }
                        >
                          {errRate.toFixed(1)}%
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{formatMs(m.ttft_avg)}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{formatMs(m.ttft_p95)}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{formatMs(m.e2e_avg)}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{formatMs(m.e2e_p95)}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {m.tpot_avg != null && m.tpot_avg > 0
                          ? `${(1000 / m.tpot_avg).toFixed(1)} tok/s`
                          : "—"}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{formatNumber(m.total_input_tokens)}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{formatNumber(m.total_output_tokens)}</td>
                      <td className="px-2 py-2.5">
                        <ChevronRight
                          className={cn(
                            "size-4 text-muted-foreground transition-transform",
                            isSelected && "rotate-90",
                          )}
                        />
                      </td>
                    </tr>
                  )
                })
              )}
            </tbody>
          </table>
        </div>
      </div>

      {/* Selected Model Detail Charts */}
      {selectedModel && <ModelDetailCharts model={selectedModel} />}
    </div>
  )
}
