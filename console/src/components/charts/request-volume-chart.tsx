import {
  AreaChart,
  Area,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from "recharts"
import { formatNumber } from "@/lib/format"
import type { TimeseriesData } from "@/types/api"

// Stable color palette for wire APIs
const SERIES_COLORS = [
  "#3b82f6", // blue
  "#10b981", // emerald
  "#f59e0b", // amber
  "#ef4444", // red
  "#8b5cf6", // violet
  "#ec4899", // pink
  "#06b6d4", // cyan
  "#84cc16", // lime
]

function formatAxisTime(epoch: number): string {
  const d = new Date(epoch * 1000)
  const hh = String(d.getHours()).padStart(2, "0")
  const mm = String(d.getMinutes()).padStart(2, "0")
  return `${hh}:${mm}`
}

interface Props {
  data: TimeseriesData | null
}

export function RequestVolumeChart({ data }: Props) {
  if (!data || data.timestamps.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No data available
      </div>
    )
  }

  // Extract wire-api groups from series
  const requestSeries = data.series.filter((s) => s.name === "request_count" && s.group)
  const groups = requestSeries.map((s) => s.group!)

  // Build chart data: [{time, group1: val, group2: val, ...}]
  const chartData = data.timestamps.map((ts, i) => {
    const point: Record<string, number> = { time: ts }
    for (const series of requestSeries) {
      point[series.group!] = series.values[i] ?? 0
    }
    return point
  })

  return (
    <ResponsiveContainer width="100%" height={240}>
      <AreaChart data={chartData} margin={{ top: 4, right: 8, bottom: 0, left: -12 }}>
        <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
        <XAxis
          dataKey="time"
          tickFormatter={formatAxisTime}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
        />
        <YAxis
          tickFormatter={(v: number) => formatNumber(v)}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
        />
        <Tooltip
          labelFormatter={(v) => new Date(Number(v) * 1000).toLocaleString()}
          formatter={(value, name) => [formatNumber(Number(value)), String(name)]}
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            borderColor: "hsl(var(--border))",
            borderRadius: "8px",
            fontSize: "12px",
          }}
        />
        <Legend wrapperStyle={{ fontSize: "12px" }} />
        {groups.map((group, i) => (
          <Area
            key={group}
            type="monotone"
            dataKey={group}
            stackId="1"
            fill={SERIES_COLORS[i % SERIES_COLORS.length]}
            stroke={SERIES_COLORS[i % SERIES_COLORS.length]}
            fillOpacity={0.4}
          />
        ))}
      </AreaChart>
    </ResponsiveContainer>
  )
}
