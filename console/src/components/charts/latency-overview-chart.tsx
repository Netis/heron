import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from "recharts"
import { formatMs } from "@/lib/format"
import type { TimeseriesData } from "@/types/api"

const SERIES_CONFIG = [
  { key: "ttft_avg", label: "TTFT p50", color: "#f59e0b", dash: undefined },
  { key: "ttft_p95", label: "TTFT p95", color: "#f59e0b", dash: "5 3" },
  { key: "e2e_avg", label: "E2E p50", color: "#3b82f6", dash: undefined },
  { key: "e2e_p95", label: "E2E p95", color: "#3b82f6", dash: "5 3" },
]

function formatAxisTime(epoch: number): string {
  const d = new Date(epoch * 1000)
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`
}

interface Props {
  data: TimeseriesData | null
}

export function LatencyOverviewChart({ data }: Props) {
  if (!data || data.timestamps.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No data available
      </div>
    )
  }

  // Build chart data
  const chartData = data.timestamps.map((ts, i) => {
    const point: Record<string, number | null> = { time: ts }
    for (const series of data.series) {
      point[series.name] = series.values[i]
    }
    return point
  })

  return (
    <ResponsiveContainer width="100%" height={240}>
      <LineChart data={chartData} margin={{ top: 4, right: 8, bottom: 0, left: 8 }}>
        <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
        <XAxis
          dataKey="time"
          tickFormatter={formatAxisTime}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
        />
        <YAxis
          tickFormatter={(v: number) => formatMs(v)}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
          width={70}
        />
        <Tooltip
          labelFormatter={(v) => new Date(Number(v) * 1000).toLocaleString()}
          formatter={(value, name) => {
            const config = SERIES_CONFIG.find((s) => s.key === name)
            return [formatMs(Number(value)), config?.label ?? String(name)]
          }}
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            borderColor: "hsl(var(--border))",
            borderRadius: "8px",
            fontSize: "12px",
          }}
        />
        <Legend
          wrapperStyle={{ fontSize: "12px" }}
          formatter={(value: string) => SERIES_CONFIG.find((s) => s.key === value)?.label ?? value}
        />
        {SERIES_CONFIG.map((cfg) => (
          <Line
            key={cfg.key}
            type="monotone"
            dataKey={cfg.key}
            stroke={cfg.color}
            strokeDasharray={cfg.dash}
            strokeWidth={2}
            dot={false}
            connectNulls
          />
        ))}
      </LineChart>
    </ResponsiveContainer>
  )
}
