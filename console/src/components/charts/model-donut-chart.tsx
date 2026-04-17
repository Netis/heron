import { PieChart, Pie, Cell, Tooltip, ResponsiveContainer, Legend } from "recharts"
import { formatNumber } from "@/lib/format"
import type { MetricsModelRow } from "@/types/api"

const COLORS = [
  "#3b82f6", // blue
  "#10b981", // emerald
  "#f59e0b", // amber
  "#ef4444", // red
  "#8b5cf6", // violet
  "#ec4899", // pink
  "#06b6d4", // cyan
  "#84cc16", // lime
]

interface Props {
  models: MetricsModelRow[]
  height?: number
}

export function ModelDonutChart({ models, height = 240 }: Props) {
  if (models.length === 0) {
    return (
      <div
        className="flex items-center justify-center text-sm text-muted-foreground"
        style={{ height }}
      >
        No data available
      </div>
    )
  }

  const sorted = [...models].sort((a, b) => b.request_count - a.request_count)
  // Show top 7, group rest into "Other"
  const top = sorted.slice(0, 7)
  const rest = sorted.slice(7)
  const data = top.map((m) => ({ name: m.model, value: m.request_count }))
  if (rest.length > 0) {
    const otherCount = rest.reduce((sum, m) => sum + m.request_count, 0)
    data.push({ name: "Other", value: otherCount })
  }

  const total = data.reduce((sum, d) => sum + d.value, 0)

  return (
    <ResponsiveContainer width="100%" height={height}>
      <PieChart>
        <Pie
          data={data}
          cx="50%"
          cy="50%"
          innerRadius={60}
          outerRadius={90}
          paddingAngle={2}
          dataKey="value"
          label={({ name, percent }) =>
            `${name} ${((percent ?? 0) * 100).toFixed(0)}%`
          }
          labelLine={{ strokeWidth: 1 }}
          style={{ fontSize: "11px" }}
        >
          {data.map((_, i) => (
            <Cell key={i} fill={COLORS[i % COLORS.length]} />
          ))}
        </Pie>
        <Tooltip
          formatter={(value) => [
            `${formatNumber(Number(value))} (${((Number(value) / total) * 100).toFixed(1)}%)`,
            "Requests",
          ]}
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            borderColor: "hsl(var(--border))",
            borderRadius: "8px",
            fontSize: "12px",
          }}
        />
        <Legend wrapperStyle={{ fontSize: "12px" }} />
      </PieChart>
    </ResponsiveContainer>
  )
}
