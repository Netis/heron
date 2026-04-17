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
import type { MetricsModelRow } from "@/types/api"

interface Props {
  models: MetricsModelRow[]
}

export function ErrorByModelChart({ models }: Props) {
  if (models.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No data available
      </div>
    )
  }

  // Sort by error rate descending
  const withRate = models
    .map((m) => ({
      model: m.model.length > 24 ? m.model.slice(0, 22) + "..." : m.model,
      fullModel: m.model,
      total: m.request_count,
      "4xx": m.request_count > 0 ? ((m.error_4xx_count - m.error_429_count) / m.request_count) * 100 : 0,
      "429": m.request_count > 0 ? (m.error_429_count / m.request_count) * 100 : 0,
      "5xx": m.request_count > 0 ? (m.error_5xx_count / m.request_count) * 100 : 0,
      errorRate: m.request_count > 0 ? (m.error_count / m.request_count) * 100 : 0,
    }))
    .sort((a, b) => b.errorRate - a.errorRate)
    .slice(0, 10)

  return (
    <ResponsiveContainer width="100%" height={Math.max(240, withRate.length * 36 + 40)}>
      <BarChart data={withRate} layout="vertical" margin={{ top: 4, right: 8, bottom: 0, left: 0 }}>
        <CartesianGrid strokeDasharray="3 3" horizontal={false} className="stroke-border" />
        <XAxis
          type="number"
          domain={[0, "auto"]}
          tickFormatter={(v: number) => `${v.toFixed(0)}%`}
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
          formatter={(value, name) => [`${Number(value).toFixed(2)}%`, String(name)]}
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
        <Legend wrapperStyle={{ fontSize: "12px" }} />
        <Bar dataKey="4xx" stackId="err" fill="#f59e0b" radius={0} barSize={20} />
        <Bar dataKey="429" stackId="err" fill="#ef4444" radius={0} barSize={20} />
        <Bar dataKey="5xx" stackId="err" fill="#dc2626" radius={[0, 4, 4, 0]} barSize={20} />
      </BarChart>
    </ResponsiveContainer>
  )
}
