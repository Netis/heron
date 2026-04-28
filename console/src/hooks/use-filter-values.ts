import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"

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

function useFilterValues(endpoint: string, opts?: Options) {
  return useQuery({
    queryKey: ["filter-values", endpoint],
    queryFn: () => apiFetch<FilterValuesData>(endpoint),
    staleTime: 60_000,
    enabled: opts?.enabled ?? true,
  })
}

export function useWireApis(opts?: Options) {
  return useFilterValues("/api/filters/wire-apis", opts)
}

export function useModelNames(opts?: Options) {
  return useFilterValues("/api/filters/models", opts)
}

export function useServerIps(opts?: Options) {
  return useFilterValues("/api/filters/server-ips", opts)
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
