import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import type { RuntimeConfigResponse } from "@/types/api"

/**
 * Fetch /api/runtime-config — the in-memory configuration of the running
 * Heron process. The config can only change with a restart, so there
 * is no polling; the page exposes a manual Refresh button via `refetch`.
 */
export function useRuntimeConfig() {
  return useQuery({
    queryKey: ["runtime-config"],
    queryFn: () => apiFetch<RuntimeConfigResponse>("/api/runtime-config"),
    staleTime: Infinity,
    refetchOnMount: false,
    refetchOnWindowFocus: false,
    retry: 1,
  })
}
