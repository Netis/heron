import type { TimeseriesData } from "@/types/api"

/** Seconds covered by one bucket at each granularity the API serves. */
export const GRANULARITY_SECONDS: Record<string, number> = {
  "10s": 10,
  "1m": 60,
  "5m": 300,
  "1h": 3600,
}

/** The bucket size the charts use for a window of `rangeSeconds`. */
export function pickGranularity(rangeSeconds: number): string {
  if (rangeSeconds <= 900) return "10s"
  if (rangeSeconds <= 7200) return "1m"
  if (rangeSeconds <= 86400) return "5m"
  return "1h"
}

/**
 * Turn per-bucket totals into per-second rates.
 *
 * A bucket holds the sum over its whole span, so the chart of a raw total
 * changes height when the granularity changes (a 5m bucket is five 1m
 * buckets stacked). Dividing by the span makes the series comparable across
 * zoom levels. `null` (no data) stays `null` — it is not a zero rate.
 *
 * A bucket that has not ended by `windowEnd` (epoch seconds) is dropped to
 * `null`. Its total is still filling — and a call's tokens are only known
 * when it completes — so as a rate it reads as a collapse at the right edge
 * of every live chart.
 */
export function toPerSecond(
  data: TimeseriesData,
  granularity: string,
  windowEnd: number,
): TimeseriesData {
  const secs = GRANULARITY_SECONDS[granularity]
  if (!secs) throw new Error(`unknown granularity: ${granularity}`)
  const open = data.timestamps.map((ts) => ts + secs > windowEnd)
  return {
    ...data,
    series: data.series.map((s) => ({
      ...s,
      values: s.values.map((v, i) => (v == null || open[i] ? null : v / secs)),
    })),
  }
}
