import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"

interface FilterValuesData {
  values: string[]
}

function useFilterValues(endpoint: string) {
  return useQuery({
    queryKey: ["filter-values", endpoint],
    queryFn: () => apiFetch<FilterValuesData>(endpoint),
    staleTime: 60_000,
  })
}

export function useWireApis() {
  return useFilterValues("/api/filters/wire-apis")
}

export function useModelNames() {
  return useFilterValues("/api/filters/models")
}

export function useServerIps() {
  return useFilterValues("/api/filters/server-ips")
}
