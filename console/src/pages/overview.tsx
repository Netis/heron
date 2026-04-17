import { Loader2 } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { useMetricsSummary, useTimeseries, useModels } from "@/hooks/use-metrics"
import { RequestVolumeChart } from "@/components/charts/request-volume-chart"
import { LatencyOverviewChart } from "@/components/charts/latency-overview-chart"
import { ModelBreakdownChart } from "@/components/charts/model-breakdown-chart"
import { ErrorByModelChart } from "@/components/charts/error-by-model-chart"

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

function errorRateColor(rate: number): "green" | "amber" | "red" {
  if (rate < 1) return "green"
  if (rate < 5) return "amber"
  return "red"
}

export function OverviewPage() {
  const { data: summary, isLoading: summaryLoading } = useMetricsSummary()
  const { data: volumeTs } = useTimeseries("request_count", { groupBy: "provider" })
  const { data: latencyTs } = useTimeseries("ttfb_avg,ttfb_p95,e2e_avg,e2e_p95")
  const { data: modelsData } = useModels()

  if (summaryLoading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  const errorRate =
    summary && summary.request_count > 0
      ? (summary.error_count / summary.request_count) * 100
      : 0

  const totalTokens = (summary?.total_input_tokens ?? 0) + (summary?.total_output_tokens ?? 0)

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* KPI Cards */}
      <div className="grid grid-cols-6 gap-3">
        <KpiCard
          title="Total Requests"
          value={formatNumber(summary?.request_count ?? 0)}
        />
        <KpiCard
          title="Avg TTFB"
          value={formatMs(summary?.ttfb_avg)}
        />
        <KpiCard
          title="Avg E2E Latency"
          value={formatMs(summary?.e2e_avg)}
        />
        <KpiCard
          title="Error Rate"
          value={`${errorRate.toFixed(2)}%`}
          color={errorRateColor(errorRate)}
        />
        <KpiCard
          title="Total Tokens"
          value={formatNumber(totalTokens)}
          subtext={`${formatNumber(summary?.total_input_tokens)} in / ${formatNumber(summary?.total_output_tokens)} out`}
        />
        <KpiCard
          title="Avg TPOT"
          value={summary?.tpot_avg != null ? `${summary.tpot_avg.toFixed(1)} ms/tok` : "—"}
          subtext="streaming only"
        />
      </div>

      {/* Middle row — 2 charts */}
      <div className="grid grid-cols-2 gap-4">
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Request Volume</h3>
          <RequestVolumeChart data={volumeTs ?? null} />
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Latency Overview</h3>
          <LatencyOverviewChart data={latencyTs ?? null} />
        </div>
      </div>

      {/* Bottom row — 2 panels */}
      <div className="grid grid-cols-2 gap-4">
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Model Breakdown</h3>
          <ModelBreakdownChart models={modelsData?.models ?? []} />
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Error Rate by Model</h3>
          <ErrorByModelChart models={modelsData?.models ?? []} />
        </div>
      </div>
    </div>
  )
}
