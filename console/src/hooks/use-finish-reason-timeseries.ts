import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"

/** Long-format series for one raw provider finish_reason value. */
export interface FinishReasonSeries {
  finish_reason: string
  /** `[timestamp_us, count]` tuples — backend emits microseconds. */
  points: [number, number][]
}

export interface FinishReasonsResponse {
  series: FinishReasonSeries[]
}

/**
 * Reads the long-format finish-reason endpoint introduced in Phase 5.
 * Mirrors `useTimeseries`: pulls range/filters from the toolbar store,
 * auto-derives granularity from the selected window, and uses the same
 * envelope-unwrapping fetch wrapper.
 */
export function useFinishReasonTimeseries(opts?: { granularity?: string }) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const preset = useToolbarStore((s) => s.preset)
  const { params: fp } = useSupportedFilterParams()

  // Auto-compute granularity based on time range (matches useTimeseries).
  const rangeSeconds = end - start
  const granularity =
    opts?.granularity ??
    (rangeSeconds <= 900
      ? "10s"
      : rangeSeconds <= 7200
        ? "1m"
        : rangeSeconds <= 86400
          ? "5m"
          : "1h")

  return useQuery({
    queryKey: ["finish-reasons", { start, end, granularity, ...fp }],
    queryFn: () =>
      apiFetch<FinishReasonsResponse>("/api/metrics/finish-reasons", {
        start,
        end,
        granularity,
        ...fp,
      }),
    meta: { preset },
  })
}
