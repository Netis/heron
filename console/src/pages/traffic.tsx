import { useMemo, useState } from "react"
import { ArrowUpDown } from "lucide-react"
import { useTimeseries, useModels } from "@/hooks/use-metrics"
import {
  useFinishReasonTimeseries,
  type FinishReasonSeries,
} from "@/hooks/use-finish-reason-timeseries"
import { finishTone, type FinishTone } from "@/lib/finish-tone"
import { formatMs, formatNumber } from "@/lib/format"
import { TimeseriesLineChart } from "@/components/charts/timeseries-line-chart"
import { StackedBarChart } from "@/components/charts/stacked-bar-chart"
import { ModelDonutChart } from "@/components/charts/model-donut-chart"
import type { MetricsModelRow, TimeseriesData } from "@/types/api"

const TOKEN_USAGE_SERIES = [
  { key: "total_input_tokens", label: "Input Tokens", color: "#3b82f6" },
  { key: "total_output_tokens", label: "Output Tokens", color: "#10b981" },
]

const TOKEN_AVG_SERIES = [
  { key: "input_tokens_avg", label: "Avg Input", color: "#3b82f6" },
  { key: "output_tokens_avg", label: "Avg Output", color: "#10b981" },
]

/**
 * Tone → hex map for finish-reason chart colors. Recharts can't read Tailwind
 * classes from `lib/finish-tone`, so we mirror those tones here as the -500
 * shade of the same color families.
 */
const TONE_HEX: Record<FinishTone, string> = {
  ok: "#10b981", // emerald-500
  warn: "#f59e0b", // amber-500
  tool: "#3b82f6", // blue-500
  pause: "#0ea5e9", // sky-500
  err: "#ef4444", // red-500
  muted: "#9ca3af", // gray-400
}

interface FinishChartShape {
  series: { key: string; label: string; color: string }[]
  data: TimeseriesData
}

/**
 * Pivot the long-format finish-reasons response into the wide-row
 * `TimeseriesData` that `TimeseriesLineChart` consumes. Series are sorted
 * alphabetically so legend / stacking order stays stable as new
 * finish_reasons appear or disappear from the response.
 *
 * Backend emits microsecond timestamps; the chart component renders seconds
 * (multiplies by 1000 internally), so we divide by 1_000_000 here.
 */
function buildFinishChart(series: FinishReasonSeries[] | undefined): FinishChartShape | null {
  if (!series || series.length === 0) return null

  const sorted = [...series].sort((a, b) => a.finish_reason.localeCompare(b.finish_reason))

  // Collect unique microsecond timestamps across all series.
  const tsSet = new Set<number>()
  for (const s of sorted) for (const [ts] of s.points) tsSet.add(ts)
  if (tsSet.size === 0) return null

  const timestampsUs = [...tsSet].sort((a, b) => a - b)
  const timestamps = timestampsUs.map((us) => Math.floor(us / 1_000_000))

  const tsIndex = new Map<number, number>()
  timestampsUs.forEach((ts, i) => tsIndex.set(ts, i))

  const wideSeries = sorted.map((s) => {
    const values: (number | null)[] = new Array(timestampsUs.length).fill(null)
    for (const [ts, count] of s.points) {
      const i = tsIndex.get(ts)
      if (i !== undefined) values[i] = count
    }
    return { name: s.finish_reason, group: null, values }
  })

  const seriesConfig = sorted.map((s) => ({
    key: s.finish_reason,
    label: s.finish_reason,
    color: TONE_HEX[finishTone(s.finish_reason)],
  }))

  return {
    series: seriesConfig,
    data: { timestamps, series: wideSeries },
  }
}

function ChartCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-border/50 bg-card p-4 card-elevated">
      <h3 className="mb-3 text-sm font-medium">{title}</h3>
      {children}
    </div>
  )
}

type SortKey = "call_count" | "total_input_tokens" | "total_output_tokens" | "ttft_avg" | "e2e_avg" | "error_rate"
type SortOrder = "asc" | "desc"

function getErrorRate(m: MetricsModelRow): number {
  return m.call_count > 0 ? (m.error_count / m.call_count) * 100 : 0
}

function TopModelsTable({ models }: { models: MetricsModelRow[] }) {
  const [sortKey, setSortKey] = useState<SortKey>("call_count")
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
            <th className="px-2 py-2 text-right"><SortHeader label="Calls" field="call_count" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="In Tokens" field="total_input_tokens" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="Out Tokens" field="total_output_tokens" /></th>
            <th className="px-2 py-2 text-right"><SortHeader label="Avg TTFT" field="ttft_avg" /></th>
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
              <td className="px-2 py-2 text-right tabular-nums">{formatNumber(m.call_count)}</td>
              <td className="px-2 py-2 text-right tabular-nums">{formatNumber(m.total_input_tokens)}</td>
              <td className="px-2 py-2 text-right tabular-nums">{formatNumber(m.total_output_tokens)}</td>
              <td className="px-2 py-2 text-right tabular-nums">{formatMs(m.ttft_avg)}</td>
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
  const { data: volumeData } = useTimeseries("call_count", { groupBy: "wire_api" })
  const { data: tokenUsageData } = useTimeseries("total_input_tokens,total_output_tokens")
  const { data: finishReasonData } = useFinishReasonTimeseries()
  const { data: tokenAvgData } = useTimeseries("input_tokens_avg,output_tokens_avg")
  const { data: modelsData } = useModels()

  const finishChart = useMemo(() => buildFinishChart(finishReasonData?.series), [finishReasonData])

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* Top row */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="Call Volume by Wire API">
          <StackedBarChart data={volumeData ?? null} field="call_count" />
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
          {finishChart ? (
            <TimeseriesLineChart
              data={finishChart.data}
              series={finishChart.series}
              yFormatter={(v) => formatNumber(v)}
              variant="area"
            />
          ) : (
            <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
              No finish-reason data in this range
            </div>
          )}
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
