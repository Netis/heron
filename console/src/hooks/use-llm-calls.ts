import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { LlmCallsPage } from "@/types/api"

interface UseLlmCallsParams {
  page: number
  pageSize: number
  sortBy: string
  sortOrder: "asc" | "desc"
  /** Page-specific filters */
  statusCode?: string
  finishReason?: string
}

export function useLlmCalls({ page, pageSize, sortBy, sortOrder, statusCode, finishReason }: UseLlmCallsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const filters = useToolbarStore((s) => s.filters)

  return useQuery({
    queryKey: ["llm-calls", {
      start, end, page, pageSize, sortBy, sortOrder,
      wireApi: filters.wireApi, model: filters.model, serverIp: filters.serverIp,
      statusCode, finishReason,
    }],
    queryFn: () =>
      apiFetch<LlmCallsPage>("/api/llm-calls", {
        start,
        end,
        page,
        page_size: pageSize,
        sort_by: sortBy,
        sort_order: sortOrder,
        wire_api: filters.wireApi || undefined,
        model: filters.model || undefined,
        server_ip: filters.serverIp || undefined,
        status_code: statusCode || undefined,
        finish_reason: finishReason || undefined,
      }),
    placeholderData: (prev) => prev,
  })
}
