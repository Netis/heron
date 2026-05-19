import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { usePipelineHealthStore } from "@/stores/pipeline-health"
import type {
  InternalMetricsResponse,
  InternalMetricsSeriesResponse,
} from "@/types/api"

/**
 * Poll /api/internal-metrics. Cadence comes from the store; `null` pauses.
 *
 * The hook is stateless re: deltas — consumers compute deltas using
 * the previous query result (`previousData`) plus the current `ts`.
 */
export function useInternalMetrics() {
  const intervalMs = usePipelineHealthStore((s) => s.intervalMs)

  return useQuery({
    queryKey: ["internal-metrics"],
    queryFn: () => apiFetch<InternalMetricsResponse>("/api/internal-metrics"),
    refetchInterval: intervalMs ?? false,
    refetchIntervalInBackground: false,
    // Keep the previous frame visible during refetches so the page doesn't
    // flicker, and so consumers can compute (current - previous) deltas.
    placeholderData: (prev) => prev,
    retry: 1,
  })
}

interface SeriesOptions {
  /** Unix epoch ms cutoff (omit / 0 ⇒ full ring). */
  sinceMs?: number
  /** Short metric names to fetch (e.g. ["flows_active", "turns_active"]). */
  metrics?: string[]
  /** Poll interval; `null` to pause. Default 10s — matches the backend recorder cadence. */
  intervalMs?: number | null
}

/**
 * Poll `/api/internal-metrics/series` for gauge time-series (flows_active,
 * turns_active). The backend holds a 24h in-memory ring; this hook just
 * keeps the latest snapshot fresh.
 */
export function useInternalMetricsSeries(opts: SeriesOptions = {}) {
  const { sinceMs, metrics, intervalMs = 10_000 } = opts
  const params = new URLSearchParams()
  if (sinceMs && sinceMs > 0) params.set("since", String(sinceMs))
  if (metrics?.length) params.set("metrics", metrics.join(","))
  const qs = params.toString()
  const url = qs ? `/api/internal-metrics/series?${qs}` : "/api/internal-metrics/series"

  return useQuery({
    queryKey: ["internal-metrics-series", qs],
    queryFn: () => apiFetch<InternalMetricsSeriesResponse>(url),
    refetchInterval: intervalMs ?? false,
    refetchIntervalInBackground: false,
    placeholderData: (prev) => prev,
    retry: 1,
  })
}
