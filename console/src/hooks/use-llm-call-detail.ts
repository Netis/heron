import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import type { LlmCallDetail } from "@/types/api"

export function useLlmCallDetail(id: string | null) {
  return useQuery({
    queryKey: ["llm-call-detail", id],
    queryFn: () => apiFetch<LlmCallDetail>(`/api/llm-calls/${id}`),
    enabled: id != null,
  })
}
