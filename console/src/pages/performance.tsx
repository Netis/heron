import { Filter } from "lucide-react"
import { useTimeseries } from "@/hooks/use-metrics"
import { useSearchParamState } from "@/hooks/use-search-param-state"
import { formatMs, formatNumber } from "@/lib/format"
import { TimeseriesLineChart } from "@/components/charts/timeseries-line-chart"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import type { ToolSurface } from "@/types/api"

const SURFACE_OPTIONS: ToolSurface[] = ["function_call", "mcp", "cli", "mixed", "unknown"]

// Stream-only TTFT — non-streaming TTFT collapses to ~e2e because
// servers buffer the full response, so showing it here is just
// duplicated e2e curves. The non-streaming TTFT value still appears on
// individual call rows and in the call detail panel for per-call
// inspection.
const TTFT_SERIES = [
  { key: "ttft_stream_p50", label: "p50", color: "#f59e0b" },
  { key: "ttft_stream_p95", label: "p95", color: "#ef4444" },
  { key: "ttft_stream_p99", label: "p99", color: "#dc2626", dash: "5 3" },
]

const E2E_SERIES = [
  { key: "e2e_p50", label: "p50", color: "#3b82f6" },
  { key: "e2e_p95", label: "p95", color: "#8b5cf6" },
  { key: "e2e_p99", label: "p99", color: "#7c3aed", dash: "5 3" },
]

const TPOT_SERIES = [
  { key: "tpot_p50", label: "p50", color: "#10b981" },
  { key: "tpot_p95", label: "p95", color: "#059669" },
]

const ACTIVE_CALLS_SERIES = [
  { key: "active_calls_avg", label: "avg", color: "#3b82f6" },
  { key: "active_calls_max", label: "max", color: "#ef4444" },
]

const CACHE_TOKEN_SERIES = [
  { key: "total_cache_read_input_tokens", label: "Cache Read", color: "#3b82f6" },
  { key: "total_cache_creation_input_tokens", label: "Cache Creation", color: "#10b981" },
]

const TOKEN_AVG_SERIES = [
  { key: "input_tokens_avg", label: "Avg Input", color: "#3b82f6" },
  { key: "output_tokens_avg", label: "Avg Output", color: "#10b981" },
]

function formatActiveCalls(v: number): string {
  return v.toFixed(1)
}

function ChartCard({
  title,
  subtitle,
  children,
}: {
  title: string
  subtitle?: string
  children: React.ReactNode
}) {
  return (
    <div className="rounded-lg border border-border/50 bg-card p-4 card-elevated">
      <h3 className="text-sm font-medium">{title}</h3>
      {subtitle && (
        <p className="mb-3 text-xs text-muted-foreground">{subtitle}</p>
      )}
      {!subtitle && <div className="mb-3" />}
      {children}
    </div>
  )
}

export function PerformancePage() {
  const [surfaceStr, setSurfaceStr] = useSearchParamState("surface", "")
  const surfaceFilter = surfaceStr ? (surfaceStr.split(",") as ToolSurface[]) : []
  const toolSurface = surfaceFilter.join(",")

  const { data: ttftData } = useTimeseries(
    "ttft_stream_p50,ttft_stream_p95,ttft_stream_p99",
    { toolSurface },
  )
  const { data: e2eData } = useTimeseries("e2e_p50,e2e_p95,e2e_p99", { toolSurface })
  const { data: tpotData } = useTimeseries("tpot_p50,tpot_p95", { toolSurface })
  const { data: activeCallsData } = useTimeseries("active_calls_avg,active_calls_max", { toolSurface })
  const { data: cacheTokenData } = useTimeseries(
    "total_cache_read_input_tokens,total_cache_creation_input_tokens",
    { toolSurface },
  )
  const { data: tokenAvgData } = useTimeseries("input_tokens_avg,output_tokens_avg", { toolSurface })

  return (
    <div className="flex flex-col gap-4 p-4">
      <div className="flex items-center gap-2">
        <Filter className="size-3.5 text-muted-foreground" />
        <span className="text-xs text-muted-foreground">Filters:</span>
        <FilterDropdown
          label="Tool surface"
          options={SURFACE_OPTIONS}
          selected={surfaceFilter}
          onChange={(v) => setSurfaceStr(v.join(","))}
        />
      </div>
      {/* Top row: TTFT (stream + non-stream overlaid) + E2E */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard
          title="Stream TTFT Distribution"
          subtitle="Streaming responses only (true time to first token). Non-streaming TTFT collapses to e2e — see E2E chart."
        >
          <TimeseriesLineChart
            data={ttftData ?? null}
            series={TTFT_SERIES}
            yFormatter={formatMs}
          />
        </ChartCard>
        <ChartCard title="E2E Latency Distribution">
          <TimeseriesLineChart data={e2eData ?? null} series={E2E_SERIES} yFormatter={formatMs} />
        </ChartCard>
      </div>

      {/* Middle row: TPOT + Active calls */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="TPOT (Time Per Output Token)">
          <TimeseriesLineChart
            data={tpotData ?? null}
            series={TPOT_SERIES}
            yFormatter={formatMs}
          />
        </ChartCard>
        <ChartCard title="Active Calls">
          <TimeseriesLineChart
            data={activeCallsData ?? null}
            series={ACTIVE_CALLS_SERIES}
            yFormatter={formatActiveCalls}
            variant="area"
          />
        </ChartCard>
      </div>

      {/* Bottom row: Cache + Token averages */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="Cache Token Usage">
          <TimeseriesLineChart
            data={cacheTokenData ?? null}
            series={CACHE_TOKEN_SERIES}
            yFormatter={(v) => formatNumber(v)}
            variant="area"
          />
        </ChartCard>
        <ChartCard title="Token Averages">
          <TimeseriesLineChart
            data={tokenAvgData ?? null}
            series={TOKEN_AVG_SERIES}
            yFormatter={(v) => formatNumber(v)}
          />
        </ChartCard>
      </div>
    </div>
  )
}
