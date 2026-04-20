import { useState } from "react"
import { ArrowUpDown } from "lucide-react"
import { useTimeseries, useModels } from "@/hooks/use-metrics"
import { formatMs, formatNumber } from "@/lib/format"
import { TimeseriesLineChart } from "@/components/charts/timeseries-line-chart"
import { StackedBarChart } from "@/components/charts/stacked-bar-chart"
import { ModelDonutChart } from "@/components/charts/model-donut-chart"
import type { MetricsModelRow } from "@/types/api"

const TOKEN_USAGE_SERIES = [
  { key: "total_input_tokens", label: "Input Tokens", color: "#3b82f6" },
  { key: "total_output_tokens", label: "Output Tokens", color: "#10b981" },
]

const FINISH_REASON_SERIES = [
  { key: "finish_complete_count", label: "Complete", color: "#10b981" },
  { key: "finish_length_count", label: "Length", color: "#f59e0b" },
  { key: "finish_tool_use_count", label: "Tool Use", color: "#3b82f6" },
  { key: "finish_error_count", label: "Error", color: "#ef4444" },
  { key: "finish_cancelled_count", label: "Cancelled", color: "#6b7280" },
]

const TOKEN_AVG_SERIES = [
  { key: "input_tokens_avg", label: "Avg Input", color: "#3b82f6" },
  { key: "output_tokens_avg", label: "Avg Output", color: "#10b981" },
]

function ChartCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-3 text-sm font-medium">{title}</h3>
      {children}
    </div>
  )
}

type SortKey = "request_count" | "total_input_tokens" | "total_output_tokens" | "ttfb_avg" | "e2e_avg" | "error_rate"
type SortOrder = "asc" | "desc"

function getErrorRate(m: MetricsModelRow): number {
  return m.request_count > 0 ? (m.error_count / m.request_count) * 100 : 0
}

function TopModelsTable({ models }: { models: MetricsModelRow[] }) {
  const [sortKey, setSortKey] = useState<SortKey>("request_count")
  const [sortOrder, setSortOrder] = useState<SortOrder>("desc")

  function handleSort(key: SortKey) {
    if (key === sortKey) {
      setSortOrder(sortOrder === "asc" ? "desc" : "asc")
    } else {
      setSortKey(key)
      setSortOrder("desc")
    }
  }

  const sorted = [...models].sort((a, b) => {
    let av: number
    let bv: number
    if (sortKey === "error_rate") {
      av = getErrorRate(a)
      bv = getErrorRate(b)
    } else {
      av = (a[sortKey] as number) ?? 0
      bv = (b[sortKey] as number) ?? 0
    }
    return sortOrder === "asc" ? av - bv : bv - av
  })

  function SortHeader({ label, field }: { label: string; field: SortKey }) {
    const active = sortKey === field
    return (
      <button
        className="inline-flex items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground"
        onClick={() => handleSort(field)}
      >
        {label}
        <ArrowUpDown className={`size-3 ${active ? "text-foreground" : "opacity-40"}`} />
      </button>
    )
  }

  if (models.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No data available
      </div>
    )
  }

  return (
    <div className="max-h-[280px] overflow-auto">
      <table className="w-full text-sm">
        <thead className="sticky top-0 bg-card">
          <tr className="border-b border-border">
            <th className="py-2 pr-3 text-left text-xs font-medium text-muted-foreground">Model</th>
            <th className="px-2 py-2 text-right"><SortHeader label="Requests" field="request_count" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="In Tokens" field="total_input_tokens" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="Out Tokens" field="total_output_tokens" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="Avg TTFB" field="ttfb_avg" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="Avg E2E" field="e2e_avg" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="Error %" field="error_rate" /></th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((m) => (
            <tr key={m.model} className="border-b border-border/30 hover:bg-muted/30">
              <td className="py-2 pr-3">
                <div className="truncate font-medium" title={m.model}>{m.model}</div>
                <div className="text-xs text-muted-foreground">{m.wire_api}</div>
              </td>
              <td className="px-2 py-2 text-right tabular-nums">{formatNumber(m.request_count)}</td>
              <td className="px-2 py-2 text-right tabular-nums">{formatNumber(m.total_input_tokens)}</td>
              <td className="px-2 py-2 text-right tabular-nums">{formatNumber(m.total_output_tokens)}</td>
              <td className="px-2 py-2 text-right tabular-nums">{formatMs(m.ttfb_avg)}</td>
              <td className="px-2 py-2 text-right tabular-nums">{formatMs(m.e2e_avg)}</td>
              <td className="px-2 py-2 text-right tabular-nums">
                <span className={getErrorRate(m) > 5 ? "text-red-500" : getErrorRate(m) > 1 ? "text-amber-500" : ""}>
                  {getErrorRate(m).toFixed(1)}%
                </span>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export function TrafficPage() {
  const { data: volumeData } = useTimeseries("request_count", { groupBy: "wire_api" })
  const { data: tokenUsageData } = useTimeseries("total_input_tokens,total_output_tokens")
  const { data: finishReasonData } = useTimeseries("finish_complete_count,finish_length_count,finish_tool_use_count,finish_error_count,finish_cancelled_count")
  const { data: tokenAvgData } = useTimeseries("input_tokens_avg,output_tokens_avg")
  const { data: modelsData } = useModels()

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* Top row */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="Request Volume by Wire API">
          <StackedBarChart data={volumeData ?? null} field="request_count" />
        </ChartCard>
        <ChartCard title="Token Usage">
          <TimeseriesLineChart
            data={tokenUsageData ?? null}
            series={TOKEN_USAGE_SERIES}
            yFormatter={(v) => formatNumber(v)}
            variant="area"
          />
        </ChartCard>
      </div>

      {/* Middle row */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="Model Distribution">
          <ModelDonutChart models={modelsData?.models ?? []} />
        </ChartCard>
        <ChartCard title="Finish Reason Breakdown">
          <TimeseriesLineChart
            data={finishReasonData ?? null}
            series={FINISH_REASON_SERIES}
            yFormatter={(v) => formatNumber(v)}
            variant="area"
          />
        </ChartCard>
      </div>

      {/* Bottom row */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="Token Averages">
          <TimeseriesLineChart
            data={tokenAvgData ?? null}
            series={TOKEN_AVG_SERIES}
            yFormatter={(v) => formatNumber(v)}
          />
        </ChartCard>
        <ChartCard title="Top Models">
          <TopModelsTable models={modelsData?.models ?? []} />
        </ChartCard>
      </div>
    </div>
  )
}
