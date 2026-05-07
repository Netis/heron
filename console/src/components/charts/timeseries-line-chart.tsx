import {
  LineChart,
  AreaChart,
  Line,
  Area,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from "recharts"
import type { TimeseriesData } from "@/types/api"

interface SeriesConfig {
  key: string
  label: string
  color: string
  dash?: string
}

interface Props {
  data: TimeseriesData | null
  series: SeriesConfig[]
  yFormatter: (value: number) => string
  height?: number
  variant?: "line" | "area"
}

function formatAxisTime(epoch: number): string {
  const d = new Date(epoch * 1000)
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`
}

export function TimeseriesLineChart({
  data,
  series,
  yFormatter,
  height = 240,
  variant = "line",
}: Props) {
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

  const chartData = data.timestamps.map((ts, i) => {
    const point: Record<string, number | null> = { time: ts }
    for (const s of data.series) {
      point[s.name] = s.values[i]
    }
    return point
  })

  const ChartComponent = variant === "area" ? AreaChart : LineChart

  return (
    <ResponsiveContainer width="100%" height={height}>
      <ChartComponent data={chartData} margin={{ top: 4, right: 8, bottom: 0, left: 8 }}>
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
          formatter={(value, name) => {
            const cfg = series.find((s) => s.key === name)
            return [yFormatter(Number(value)), cfg?.label ?? String(name)]
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
          formatter={(value: string) => series.find((s) => s.key === value)?.label ?? value}
        />
        {series.map((cfg) =>
          variant === "area" ? (
            <Area
              key={cfg.key}
              type="monotone"
              dataKey={cfg.key}
              stroke={cfg.color}
              fill={cfg.color}
              fillOpacity={0.15}
              strokeWidth={2}
              dot={false}
              connectNulls
              isAnimationActive={false}
            />
          ) : (
            <Line
              key={cfg.key}
              type="monotone"
              dataKey={cfg.key}
              stroke={cfg.color}
              strokeDasharray={cfg.dash}
              strokeWidth={2}
              dot={false}
              connectNulls
              isAnimationActive={false}
            />
          ),
        )}
      </ChartComponent>
    </ResponsiveContainer>
  )
}
