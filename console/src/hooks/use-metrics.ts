import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { MetricsSummary, TimeseriesData, ModelsData } from "@/types/api"

/** Read dimension filters from toolbar store, mapping to API param names */
function useFilterParams() {
  const filters = useToolbarStore((s) => s.filters)
  return {
    wire_api: filters.wireApi || undefined,
    model: filters.model || undefined,
    server_ip: filters.serverIp || undefined,
  }
}

export function useMetricsSummary() {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const fp = useFilterParams()

  return useQuery({
    queryKey: ["metrics-summary", { start, end, ...fp }],
    queryFn: () => apiFetch<MetricsSummary>("/api/metrics/summary", { start, end, ...fp }),
  })
}

export function useTimeseries(
  fields: string,
  opts?: { groupBy?: string; granularity?: string },
) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const preset = useToolbarStore((s) => s.preset)
  const fp = useFilterParams()

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

  return useQuery({
    queryKey: ["timeseries", { start, end, fields, granularity, groupBy: opts?.groupBy, ...fp }],
    queryFn: () =>
      apiFetch<TimeseriesData>("/api/metrics/timeseries", {
        start,
        end,
        granularity,
        fields,
        group_by: opts?.groupBy,
        ...fp,
      }),
    // Include preset in dep tracking for reactivity
    meta: { preset },
  })
}

export function useModels() {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const fp = useFilterParams()

  return useQuery({
    queryKey: ["models", { start, end, ...fp }],
    queryFn: () => apiFetch<ModelsData>("/api/metrics/models", { start, end, ...fp }),
  })
}
