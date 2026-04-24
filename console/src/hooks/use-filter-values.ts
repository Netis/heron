import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"

interface FilterValuesData {
  values: string[]
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
