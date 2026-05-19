import { TimeseriesLineChart } from "@/components/charts/timeseries-line-chart"
import { formatNumber } from "@/lib/format"
import type {
  InternalMetricsSeriesResponse,
  TimeseriesData,
} from "@/types/api"

interface Props {
  /** Backend metric short name, e.g. `"flows_active"`. */
  metric: string
  /** Human-readable line label in the legend / tooltip. */
  label: string
  /** Line color. */
  color: string
  /** Full series response from `/api/internal-metrics/series`. */
  data: InternalMetricsSeriesResponse | undefined
  height?: number
}

/**
 * Single-line chart fed by `/api/internal-metrics/series`. The endpoint
 * returns unix-ms timestamps; the underlying `TimeseriesLineChart`
 * expects unix-seconds, so we divide here.
 */
export function ActiveGaugeChart({ metric, label, color, data, height = 200 }: Props) {
  const entry = data?.series.find((s) => s.name === metric)
  const ts: TimeseriesData | null = entry && entry.points.length > 0
    ? {
        timestamps: entry.points.map((p) => Math.floor(p.t / 1000)),
        series: [
          {
            name: metric,
            group: entry.group,
            values: entry.points.map((p) => p.v),
          },
        ],
      }
    : null

  return (
    <TimeseriesLineChart
      data={ts}
      series={[{ key: metric, label, color }]}
      yFormatter={(v) => formatNumber(Math.round(v))}
      height={height}
    />
  )
}
