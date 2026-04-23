import { useTimeseries } from "@/hooks/use-metrics"
import { formatMs, formatNumber } from "@/lib/format"
import { TimeseriesLineChart } from "@/components/charts/timeseries-line-chart"

const TTFT_SERIES = [
  { key: "ttft_p50", label: "p50", color: "#f59e0b" },
  { key: "ttft_p95", label: "p95", color: "#ef4444" },
  { key: "ttft_p99", label: "p99", color: "#dc2626", dash: "5 3" },
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

function ChartCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-3 text-sm font-medium">{title}</h3>
      {children}
    </div>
  )
}

export function PerformancePage() {
  const { data: ttftData } = useTimeseries("ttft_p50,ttft_p95,ttft_p99")
  const { data: e2eData } = useTimeseries("e2e_p50,e2e_p95,e2e_p99")
  const { data: tpotData } = useTimeseries("tpot_p50,tpot_p95")
  const { data: activeCallsData } = useTimeseries("active_calls_avg,active_calls_max")
  const { data: cacheTokenData } = useTimeseries("total_cache_read_input_tokens,total_cache_creation_input_tokens")
  const { data: tokenAvgData } = useTimeseries("input_tokens_avg,output_tokens_avg")

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* Top row */}
      <div className="grid grid-cols-2 gap-4">
        <ChartCard title="TTFT Distribution">
          <TimeseriesLineChart data={ttftData ?? null} series={TTFT_SERIES} yFormatter={formatMs} />
        </ChartCard>
        <ChartCard title="E2E Latency Distribution">
          <TimeseriesLineChart data={e2eData ?? null} series={E2E_SERIES} yFormatter={formatMs} />
        </ChartCard>
      </div>

      {/* Middle row */}
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

      {/* Bottom row */}
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
