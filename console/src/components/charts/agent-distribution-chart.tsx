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
import type { AgentKindSummary } from "@/types/api"

interface Props {
  rows: AgentKindSummary[]
}

export function AgentDistributionChart({ rows }: Props) {
  if (rows.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No agents observed in the selected window
      </div>
    )
  }

  // Trim to top 10 — agent_kind cardinality is usually <10 anyway,
  // but a misconfigured client can dump dozens of arbitrary strings.
  const sorted = [...rows].sort((a, b) => b.turn_count - a.turn_count).slice(0, 10)
  const chartData = sorted.map((r) => ({
    label: r.agent_kind.length > 24 ? r.agent_kind.slice(0, 22) + "…" : r.agent_kind,
    full: r.agent_kind,
    turns: r.turn_count,
    in: r.total_input_tokens,
    out: r.total_output_tokens,
  }))

  return (
    <ResponsiveContainer
      width="100%"
      height={Math.max(240, sorted.length * 36 + 40)}
    >
      <BarChart
        data={chartData}
        layout="vertical"
        margin={{ top: 4, right: 8, bottom: 0, left: 0 }}
      >
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
          dataKey="label"
          width={150}
          className="text-[11px] fill-muted-foreground"
          tickLine={false}
          axisLine={false}
        />
        <Tooltip
          formatter={(value, name) => {
            if (name === "turns") return [formatNumber(Number(value)), "Turns"]
            return [String(value), String(name)]
          }}
          labelFormatter={(_label, payload) =>
            (payload[0]?.payload as Record<string, string>)?.full ?? String(_label)
          }
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            borderColor: "hsl(var(--border))",
            borderRadius: "8px",
            fontSize: "12px",
          }}
        />
        <Bar
          dataKey="turns"
          fill="#8b5cf6"
          radius={[0, 4, 4, 0]}
          barSize={20}
          isAnimationActive={false}
        />
      </BarChart>
    </ResponsiveContainer>
  )
}
