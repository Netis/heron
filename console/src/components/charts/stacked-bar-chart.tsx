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
import { formatNumber } from "@/lib/format"
import type { TimeseriesData } from "@/types/api"

const GROUP_COLORS = [
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
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`
}

interface Props {
  data: TimeseriesData | null
  field: string
  height?: number
  yFormatter?: (v: number) => string
}

export function StackedBarChart({ data, field, height = 240, yFormatter = formatNumber }: Props) {
  if (!data || data.timestamps.length === 0) {
    return (
      <div
        className="flex items-center justify-center text-sm text-muted-foreground"
        style={{ height }}
      >
        No data available
      </div>
    )
  }

  const groupedSeries = data.series.filter((s) => s.name === field && s.group)
  const groups = groupedSeries.map((s) => s.group!)

  const chartData = data.timestamps.map((ts, i) => {
    const point: Record<string, number> = { time: ts }
    for (const s of groupedSeries) {
      point[s.group!] = s.values[i] ?? 0
    }
    return point
  })

  return (
    <ResponsiveContainer width="100%" height={height}>
      <BarChart data={chartData} margin={{ top: 4, right: 8, bottom: 0, left: 8 }}>
        <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
        <XAxis
          dataKey="time"
          tickFormatter={formatAxisTime}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
        />
        <YAxis
          tickFormatter={(v: number) => yFormatter(v)}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
          width={70}
        />
        <Tooltip
          labelFormatter={(v) => new Date(Number(v) * 1000).toLocaleString()}
          formatter={(value, name) => [yFormatter(Number(value)), String(name)]}
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            borderColor: "hsl(var(--border))",
            borderRadius: "8px",
            fontSize: "12px",
          }}
        />
        <Legend wrapperStyle={{ fontSize: "12px" }} />
        {groups.map((group, i) => (
          <Bar
            key={group}
            dataKey={group}
            stackId="1"
            fill={GROUP_COLORS[i % GROUP_COLORS.length]}
          />
        ))}
      </BarChart>
    </ResponsiveContainer>
  )
}
