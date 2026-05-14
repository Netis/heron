import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
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
 * Distinct `agent_kind` values observed in agent_turns inside the
 * toolbar's current time range. Drives the agent-sessions / agent-turns
 * filter dropdowns so options stay in sync with what was captured.
 */
export function useAgentKinds(opts?: Options) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  return useQuery({
    queryKey: ["filter-values", "/api/filters/agent-kinds", start, end],
    queryFn: () =>
      apiFetch<FilterValuesData>("/api/filters/agent-kinds", { start, end }),
    staleTime: 30_000,
    enabled: opts?.enabled ?? true,
  })
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
