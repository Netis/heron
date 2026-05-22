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
import { formatNumber, formatAxisTime } from "@/lib/format"
import type { AgentActivityPoint } from "@/types/api"

// Same palette as RequestVolumeChart — keeps the Overview row
// internally consistent.
const SERIES_COLORS = [
  "#3b82f6",
  "#10b981",
  "#f59e0b",
  "#ef4444",
  "#8b5cf6",
  "#ec4899",
  "#06b6d4",
  "#84cc16",
]

interface Props {
  points: AgentActivityPoint[]
}

export function AgentActivityChart({ points }: Props) {
  if (points.length === 0) {
    return (
      <div className="flex h-[240px] items-center justify-center text-sm text-muted-foreground">
        No agent activity in the selected window
      </div>
    )
  }

  // Pivot the long-form rows `(ts, kind, count)` into wide-form
  // recharts data: `{time: <sec>, kind1: n, kind2: n, ...}`.
  const tsSet = new Set<number>()
  const kindSet = new Set<string>()
  const byTsKind = new Map<number, Map<string, number>>()
  for (const p of points) {
    tsSet.add(p.timestamp_ms)
    kindSet.add(p.agent_kind)
    if (!byTsKind.has(p.timestamp_ms)) byTsKind.set(p.timestamp_ms, new Map())
    byTsKind.get(p.timestamp_ms)!.set(p.agent_kind, p.turn_count)
  }
  const tsAsc = [...tsSet].sort((a, b) => a - b)
  // Sort kinds by total turns desc so the dominant agent renders
  // closest to the X axis — same convention as RequestVolumeChart.
  const totals = new Map<string, number>()
  for (const p of points) {
    totals.set(p.agent_kind, (totals.get(p.agent_kind) ?? 0) + p.turn_count)
  }
  const kindsByVolume = [...kindSet].sort(
    (a, b) => (totals.get(b) ?? 0) - (totals.get(a) ?? 0),
  )
  const chartData = tsAsc.map((ms) => {
    const row: Record<string, number> = { time: Math.floor(ms / 1000) }
    const m = byTsKind.get(ms)!
    for (const k of kindsByVolume) {
      row[k] = m.get(k) ?? 0
    }
    return row
  })
  const spanSec =
    tsAsc.length > 1 ? (tsAsc[tsAsc.length - 1] - tsAsc[0]) / 1000 : 0

  return (
    <ResponsiveContainer width="100%" height={240}>
      <AreaChart data={chartData} margin={{ top: 4, right: 8, bottom: 0, left: -12 }}>
        <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
        <XAxis
          dataKey="time"
          tickFormatter={(v: number) => formatAxisTime(v, spanSec)}
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
        {kindsByVolume.map((kind, i) => (
          <Area
            key={kind}
            type="monotone"
            dataKey={kind}
            stackId="1"
            fill={SERIES_COLORS[i % SERIES_COLORS.length]}
            stroke={SERIES_COLORS[i % SERIES_COLORS.length]}
            fillOpacity={0.4}
            isAnimationActive={false}
          />
        ))}
      </AreaChart>
    </ResponsiveContainer>
  )
}
