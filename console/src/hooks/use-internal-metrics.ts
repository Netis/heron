import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { usePipelineHealthStore } from "@/stores/pipeline-health"
import type { InternalMetricsResponse } from "@/types/api"

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
