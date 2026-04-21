import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import type { HttpExchangeDetail } from "@/types/api"

export function useHttpExchange(id: string | null) {
  return useQuery({
    queryKey: ["http-exchange", id],
    queryFn: () => apiFetch<HttpExchangeDetail>(`/api/http-exchanges/${id}`),
    enabled: id != null,
  })
}
