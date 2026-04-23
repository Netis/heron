import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from "recharts"
import { formatNumber } from "@/lib/format"
import type { MetricsModelRow } from "@/types/api"

interface Props {
  models: MetricsModelRow[]
}

export function ModelBreakdownChart({ models }: Props) {
  if (models.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No data available
      </div>
    )
  }

  // Top N models by request count, show as horizontal bar
  const sorted = [...models].sort((a, b) => b.call_count - a.call_count).slice(0, 10)
  const chartData = sorted.map((m) => ({
    model: m.model.length > 24 ? m.model.slice(0, 22) + "..." : m.model,
    fullModel: m.model,
    requests: m.call_count,
    tokens: m.total_input_tokens + m.total_output_tokens,
    avgLatency: m.e2e_avg,
  }))

  return (
    <ResponsiveContainer width="100%" height={Math.max(240, sorted.length * 36 + 40)}>
      <BarChart data={chartData} layout="vertical" margin={{ top: 4, right: 8, bottom: 0, left: 0 }}>
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
          formatter={(value, name) => {
            if (name === "requests") return [formatNumber(Number(value)), "Calls"]
            return [String(value), String(name)]
          }}
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
        <Bar dataKey="requests" fill="#3b82f6" radius={[0, 4, 4, 0]} barSize={20} />
      </BarChart>
    </ResponsiveContainer>
  )
}
