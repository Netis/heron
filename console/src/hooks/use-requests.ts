import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { CallsPage } from "@/types/api"

interface UseRequestsParams {
  page: number
  pageSize: number
  sortBy: string
  sortOrder: "asc" | "desc"
  /** Page-specific filters */
  statusCode?: string
  finishReason?: string
}

export function useRequests({ page, pageSize, sortBy, sortOrder, statusCode, finishReason }: UseRequestsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const filters = useToolbarStore((s) => s.filters)

  return useQuery({
    queryKey: ["calls", {
      start, end, page, pageSize, sortBy, sortOrder,
      provider: filters.provider, model: filters.model, serverIp: filters.serverIp,
      statusCode, finishReason,
    }],
    queryFn: () =>
      apiFetch<CallsPage>("/api/calls", {
        start,
        end,
        page,
        page_size: pageSize,
        sort_by: sortBy,
        sort_order: sortOrder,
        provider: filters.provider || undefined,
        model: filters.model || undefined,
        server_ip: filters.serverIp || undefined,
        status_code: statusCode || undefined,
        finish_reason: finishReason || undefined,
      }),
    placeholderData: (prev) => prev,
  })
}
