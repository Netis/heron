import { Loader2 } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { useMetricsSummary, useTimeseries, useModels } from "@/hooks/use-metrics"
import { useAgentActivity, useAgentSummary } from "@/hooks/use-agent-overview"
import { useInternalMetricsSeries } from "@/hooks/use-internal-metrics"
import { useToolbarStore } from "@/stores/toolbar"
import { RequestVolumeChart } from "@/components/charts/request-volume-chart"
import { LatencyOverviewChart } from "@/components/charts/latency-overview-chart"
import { ModelBreakdownChart } from "@/components/charts/model-breakdown-chart"
import { ErrorByModelChart } from "@/components/charts/error-by-model-chart"
import { AgentActivityChart } from "@/components/charts/agent-activity-chart"
import { AgentDistributionChart } from "@/components/charts/agent-distribution-chart"
import { ActiveGaugeChart } from "@/components/charts/active-gauge-chart"

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
  const { data: volumeTs } = useTimeseries("call_count", { groupBy: "wire_api" })
  const { data: latencyTs } = useTimeseries("ttft_avg,ttft_p95,e2e_avg,e2e_p95")
  const { data: modelsData } = useModels()
  const { data: agentActivity } = useAgentActivity()
  const { data: agentSummary } = useAgentSummary()
  // Toolbar exposes `start` as unix-seconds; convert to ms for the series API.
  const start = useToolbarStore((s) => s.start)
  const { data: gauges } = useInternalMetricsSeries({
    metrics: ["flows_active", "agent_turns_open"],
    sinceMs: start * 1000,
  })

  if (summaryLoading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  const errorRate =
    summary && summary.call_count > 0
      ? (summary.error_count / summary.call_count) * 100
      : 0

  const totalTokens = (summary?.total_input_tokens ?? 0) + (summary?.total_output_tokens ?? 0)

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* KPI Cards */}
      <div className="grid grid-cols-6 gap-3">
        <KpiCard
          title="Total Calls"
          value={formatNumber(summary?.call_count ?? 0)}
        />
        <KpiCard
          title="Avg TTFT"
          value={formatMs(summary?.ttft_avg)}
        />
        <KpiCard
          title="Avg E2E Latency"
          value={formatMs(summary?.e2e_avg)}
        />
        <KpiCard
          title="Call Error Rate"
          value={`${errorRate.toFixed(2)}%`}
          color={errorRateColor(errorRate)}
        />
        <KpiCard
          title="Total Tokens"
          value={formatNumber(totalTokens)}
          subtext={`${formatNumber(summary?.total_input_tokens)} in / ${formatNumber(summary?.total_output_tokens)} out`}
        />
        <KpiCard
          title="Avg TPS"
          value={
            summary?.tpot_avg != null && summary.tpot_avg > 0
              ? `${(1000 / summary.tpot_avg).toFixed(1)} tok/s`
              : "—"
          }
          subtext="streaming only · generation speed"
        />
      </div>

      {/* Middle row — 2 charts */}
      <div className="grid grid-cols-2 gap-4">
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Call Volume</h3>
          <RequestVolumeChart data={volumeTs ?? null} />
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Latency Overview</h3>
          <LatencyOverviewChart data={latencyTs ?? null} />
        </div>
      </div>

      {/* Agent row — activity timeseries + distribution */}
      <div className="grid grid-cols-2 gap-4">
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Agent Activity</h3>
          <AgentActivityChart points={agentActivity?.points ?? []} />
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Agent Distribution</h3>
          <AgentDistributionChart rows={agentSummary?.summary ?? []} />
        </div>
      </div>

      {/* Active concurrency — live gauges from internal_metrics ring */}
      <div className="grid grid-cols-2 gap-4">
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">
            Active TCP Connections
            <span className="ml-2 text-xs font-normal text-muted-foreground">
              tracked flows across all pipelines
            </span>
          </h3>
          <ActiveGaugeChart
            metric="flows_active"
            label="Active connections"
            color="#3b82f6"
            data={gauges}
          />
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">
            Active Agent Turns
            <span className="ml-2 text-xs font-normal text-muted-foreground">
              in-progress agent turns (registry size)
            </span>
          </h3>
          <ActiveGaugeChart
            metric="agent_turns_open"
            label="Open turns"
            color="#10b981"
            data={gauges}
          />
        </div>
      </div>

      {/* Bottom row — 2 panels */}
      <div className="grid grid-cols-2 gap-4">
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Model Breakdown</h3>
          <ModelBreakdownChart models={modelsData?.models ?? []} />
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-3 text-sm font-medium">Call Error Rate by Model</h3>
          <ErrorByModelChart models={modelsData?.models ?? []} />
        </div>
      </div>
    </div>
  )
}
