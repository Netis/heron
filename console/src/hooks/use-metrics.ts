import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"
import { pickGranularity, toPerSecond } from "@/lib/timeseries-rate"
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
  opts?: {
    groupBy?: string
    granularity?: string
    toolSurface?: string
    /** Divide each bucket's total by the bucket's span — for summed fields only. */
    perSecond?: boolean
  },
) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const preset = useToolbarStore((s) => s.preset)
  const { params: fp } = useSupportedFilterParams()

  const granularity = opts?.granularity ?? pickGranularity(end - start)
  const perSecond = opts?.perSecond ?? false

  const toolSurface = opts?.toolSurface && opts.toolSurface.length > 0 ? opts.toolSurface : undefined

  return useQuery({
    queryKey: [
      "timeseries",
      { start, end, fields, granularity, groupBy: opts?.groupBy, toolSurface, perSecond, ...fp },
    ],
    // Converted inside the fetch, not in `select`: the placeholder shown while
    // a new range loads is the previous range's data, and `select` would divide
    // it by the *new* range's bucket span.
    queryFn: async () => {
      const d = await apiFetch<TimeseriesData>("/api/metrics/timeseries", {
        start,
        end,
        granularity,
        fields,
        group_by: opts?.groupBy,
        tool_surface: toolSurface,
        ...fp,
      })
      return perSecond ? toPerSecond(d, granularity, end) : d
    },
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
