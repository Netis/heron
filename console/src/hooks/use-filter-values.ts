import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"
import { useToolbarStore } from "@/stores/toolbar"

interface FilterValuesData {
  values: string[]
}

export interface FinishReasonPair {
  wire_api: string
  finish_reason: string
}

interface FinishReasonPairsData {
  pairs: FinishReasonPair[]
}

interface Options {
  enabled?: boolean
}

interface AgentKindOptions extends Options {
  includeProxyHops?: boolean
}

function useFilterValues(
  endpoint: string,
  params?: Record<string, string | number | boolean | undefined>,
  opts?: Options,
) {
  return useQuery({
    queryKey: ["filter-values", endpoint, params ?? {}],
    queryFn: () => apiFetch<FilterValuesData>(endpoint, params),
    staleTime: 60_000,
    enabled: opts?.enabled ?? true,
  })
}

export function useWireApis(opts?: Options) {
  return useFilterValues("/api/filters/wire-apis", undefined, opts)
}

export function useModelNames(opts?: Options) {
  return useFilterValues("/api/filters/models", undefined, opts)
}

export function useServerIps(opts?: Options) {
  return useFilterValues("/api/filters/server-ips", undefined, opts)
}

export function useAgentKinds(opts?: AgentKindOptions) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const { params: fp } = useSupportedFilterParams()

  return useFilterValues(
    "/api/filters/agent-kinds",
    {
      start,
      end,
      ...fp,
      include_proxy_hops: opts?.includeProxyHops ? true : undefined,
    },
    opts,
  )
}

/**
 * Distinct (wire_api, finish_reason) pairs observed in the metrics table.
 * Powers the calls-page finish-reason dropdown — values are raw provider
 * strings; the dropdown groups them by wire_api on the client side.
 */
export function useFinishReasons(opts?: Options) {
  return useQuery({
    queryKey: ["filter-values", "/api/filters/finish-reasons"],
    queryFn: () => apiFetch<FinishReasonPairsData>("/api/filters/finish-reasons"),
    staleTime: 60_000,
    enabled: opts?.enabled ?? true,
  })
}
