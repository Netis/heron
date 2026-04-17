import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import type { CallDetail } from "@/types/api"

export function useRequestDetail(id: string | null) {
  return useQuery({
    queryKey: ["call-detail", id],
    queryFn: () => apiFetch<CallDetail>(`/api/calls/${id}`),
    enabled: id != null,
  })
}
