import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import type { SourcesResponse } from "@/types/api"

/** 5 s poll cadence matches the server's internal_metrics default and is
 *  finer-grained than the operator needs for a rough "online/idle/offline"
 *  signal. Users feel near-real-time; network load is negligible. */
export function useSources() {
  return useQuery({
    queryKey: ["sources"],
    queryFn: () => apiFetch<SourcesResponse>("/api/sources"),
    refetchInterval: 5000,
  })
}
