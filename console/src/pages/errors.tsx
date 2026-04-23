import { Loader2 } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatNumber } from "@/lib/format"
import { useMetricsSummary, useTimeseries, useModels } from "@/hooks/use-metrics"
import { TimeseriesLineChart } from "@/components/charts/timeseries-line-chart"
import { ErrorByModelChart } from "@/components/charts/error-by-model-chart"
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from "recharts"
import type { MetricsModelRow } from "@/types/api"

const ERROR_TIMELINE_SERIES = [
  { key: "error_4xx_count", label: "4xx", color: "#f59e0b" },
  { key: "error_429_count", label: "429", color: "#ef4444" },
  { key: "error_5xx_count", label: "5xx", color: "#dc2626" },
]

const RATE_LIMIT_SERIES = [
  { key: "error_429_count", label: "429 Rate Limited", color: "#ef4444" },
]

function KpiCard({
  title,
  value,
  subtext,
  color,
}: {
  title: string
  value: string
  subtext?: string
  color?: "green" | "amber" | "red" | "default"
}) {
  const valueColor =
    color === "green"
      ? "text-emerald-600 dark:text-emerald-400"
      : color === "amber"
        ? "text-amber-600 dark:text-amber-400"
        : color === "red"
          ? "text-red-600 dark:text-red-400"
          : "text-foreground"

  return (
    <div className="flex flex-col gap-1 rounded-lg border border-border bg-card p-4">
      <span className="text-xs font-medium text-muted-foreground">{title}</span>
      <span className={cn("text-2xl font-semibold tabular-nums", valueColor)}>{value}</span>
      {subtext && <span className="text-xs text-muted-foreground">{subtext}</span>}
    </div>
  )
}

function ChartCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-3 text-sm font-medium">{title}</h3>
      {children}
    </div>
  )
}

function errorRateColor(rate: number): "green" | "amber" | "red" {
  if (rate < 1) return "green"
  if (rate < 5) return "amber"
  return "red"
}

function ErrorByModelCountChart({ models }: { models: MetricsModelRow[] }) {
  const withErrors = models
    .filter((m) => m.error_count > 0)
    .map((m) => ({
      model: m.model.length > 24 ? m.model.slice(0, 22) + "..." : m.model,
      fullModel: m.model,
      "4xx": m.error_4xx_count - m.error_429_count,
      "429": m.error_429_count,
      "5xx": m.error_5xx_count,
    }))
    .sort((a, b) => (b["4xx"] + b["429"] + b["5xx"]) - (a["4xx"] + a["429"] + a["5xx"]))
    .slice(0, 10)

  if (withErrors.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No errors in selected range
      </div>
    )
  }

  return (
    <ResponsiveContainer width="100%" height={Math.max(240, withErrors.length * 36 + 40)}>
      <BarChart data={withErrors} layout="vertical" margin={{ top: 4, right: 8, bottom: 0, left: 0 }}>
        <CartesianGrid strokeDasharray="3 3" horizontal={false} className="stroke-border" />
        <XAxis
          type="number"
          tickFormatter={(v: number) => formatNumber(v)}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
        />
        <YAxis
          type="category"
          dataKey="model"
          width={150}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
        />
        <Tooltip
          formatter={(value, name) => [formatNumber(Number(value)), String(name)]}
          labelFormatter={(_label, payload) =>
            (payload[0]?.payload as Record<string, string>)?.fullModel ?? String(_label)
          }
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            borderColor: "hsl(var(--border))",
            borderRadius: "8px",
            fontSize: "12px",
          }}
        />
        <Legend wrapperStyle={{ fontSize: "12px" }} />
        <Bar dataKey="4xx" stackId="err" fill="#f59e0b" barSize={20} />
        <Bar dataKey="429" stackId="err" fill="#ef4444" barSize={20} />
        <Bar dataKey="5xx" stackId="err" fill="#dc2626" radius={[0, 4, 4, 0]} barSize={20} />
      </BarChart>
    </ResponsiveContainer>
  )
}

export function ErrorsPage() {
  const { data: summary, isLoading: summaryLoading } = useMetricsSummary()
  const { data: errorTimelineData } = useTimeseries("error_4xx_count,error_429_count,error_5xx_count")
  const { data: rateLimitData } = useTimeseries("error_429_count")
  const { data: modelsData } = useModels()

  if (summaryLoading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  const totalErrors = summary?.error_count ?? 0
  const totalRequests = summary?.call_count ?? 0
  const errorRate = totalRequests > 0 ? (totalErrors / totalRequests) * 100 : 0
  const error4xx = summary?.error_4xx_count ?? 0
  const error429 = summary?.error_429_count ?? 0
  const error5xx = summary?.error_5xx_count ?? 0

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* KPI Cards */}
      <div className="grid grid-cols-4 gap-3">
        <KpiCard
          title="Total Errors"
          value={formatNumber(totalErrors)}
          subtext={`${errorRate.toFixed(2)}% of all calls`}
          color={errorRateColor(errorRate)}
        />
        <KpiCard
          title="4xx Errors"
          value={formatNumber(error4xx)}
          subtext={error429 > 0 ? `incl. ${formatNumber(error429)} rate-limited (429)` : "no rate limiting"}
        />
        <KpiCard
          title="5xx Errors"
          value={formatNumber(error5xx)}
          color={error5xx > 0 ? "red" : "default"}
        />
        <KpiCard
          title="Error Rate"
          value={`${errorRate.toFixed(2)}%`}
          color={errorRateColor(errorRate)}
        />
      </div>

      {/* Middle row — 2 charts */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="Error Timeline">
          <TimeseriesLineChart
            data={errorTimelineData ?? null}
            series={ERROR_TIMELINE_SERIES}
            yFormatter={(v) => formatNumber(v)}
            variant="area"
          />
        </ChartCard>
        <ChartCard title="Error by Model">
          <ErrorByModelCountChart models={modelsData?.models ?? []} />
        </ChartCard>
      </div>

      {/* Bottom row — 2 charts */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="Error Rate by Model">
          <ErrorByModelChart models={modelsData?.models ?? []} />
        </ChartCard>
        <ChartCard title="429 Rate Limiting Trend">
          <TimeseriesLineChart
            data={rateLimitData ?? null}
            series={RATE_LIMIT_SERIES}
            yFormatter={(v) => formatNumber(v)}
          />
        </ChartCard>
      </div>
    </div>
  )
}
