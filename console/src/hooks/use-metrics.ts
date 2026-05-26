import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"
import type { MetricsSummary, TimeseriesData, ModelsData } from "@/types/api"

export function useMetricsSummary() {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const { params: fp } = useSupportedFilterParams()

  return useQuery({
    queryKey: ["metrics-summary", { start, end, ...fp }],
    queryFn: () => apiFetch<MetricsSummary>("/api/metrics/summary", { start, end, ...fp }),
    placeholderData: (prev) => prev,
  })
}

export function useTimeseries(
  fields: string,
  opts?: { groupBy?: string; granularity?: string; toolSurface?: string },
) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const preset = useToolbarStore((s) => s.preset)
  const { params: fp } = useSupportedFilterParams()

  // Auto-compute granularity based on time range
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

  const toolSurface = opts?.toolSurface && opts.toolSurface.length > 0 ? opts.toolSurface : undefined

  return useQuery({
    queryKey: ["timeseries", { start, end, fields, granularity, groupBy: opts?.groupBy, toolSurface, ...fp }],
    queryFn: () =>
      apiFetch<TimeseriesData>("/api/metrics/timeseries", {
        start,
        end,
        granularity,
        fields,
        group_by: opts?.groupBy,
        tool_surface: toolSurface,
        ...fp,
      }),
    // Include preset in dep tracking for reactivity
    meta: { preset },
    placeholderData: (prev) => prev,
  })
}

export function useModels() {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const { params: fp } = useSupportedFilterParams()

  return useQuery({
    queryKey: ["models", { start, end, ...fp }],
    queryFn: () => apiFetch<ModelsData>("/api/metrics/models", { start, end, ...fp }),
    placeholderData: (prev) => prev,
  })
}
