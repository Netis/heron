import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import type { CaptureInterfacesResponse } from "@/types/api"

/**
 * Fetch /api/capture/interfaces — the network interfaces libpcap can
 * enumerate inside the tokenscope process. List changes are rare; refresh
 * is manual via `refetch`.
 */
export function useCaptureInterfaces() {
  return useQuery({
    queryKey: ["capture-interfaces"],
    queryFn: () => apiFetch<CaptureInterfacesResponse>("/api/capture/interfaces"),
    staleTime: Infinity,
    refetchOnMount: false,
    refetchOnWindowFocus: false,
    retry: 1,
  })
}
