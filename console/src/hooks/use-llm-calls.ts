import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"
import type { LlmCallsPage } from "@/types/api"

interface UseLlmCallsParams {
  page: number
  pageSize: number
  sortBy: string
  sortOrder: "asc" | "desc"
  /** Page-specific filters */
  statusCode?: string
  finishReason?: string
  clientIp?: string
  requestPath?: string
  errorsOnly?: boolean
}

export function useLlmCalls({
  page,
  pageSize,
  sortBy,
  sortOrder,
  statusCode,
  finishReason,
  clientIp,
  requestPath,
  errorsOnly,
}: UseLlmCallsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const { params: fp } = useSupportedFilterParams()

  return useQuery({
    queryKey: ["llm-calls", {
      start, end, page, pageSize, sortBy, sortOrder,
      ...fp,
      statusCode, finishReason, clientIp, requestPath, errorsOnly,
    }],
    queryFn: () =>
      apiFetch<LlmCallsPage>("/api/llm-calls", {
        start,
        end,
        page,
        page_size: pageSize,
        sort_by: sortBy,
        sort_order: sortOrder,
        ...fp,
        status_code: statusCode || undefined,
        finish_reason: finishReason || undefined,
        client_ip: clientIp || undefined,
        request_path: requestPath || undefined,
        errors_only: errorsOnly ? true : undefined,
      }),
    placeholderData: (prev) => prev,
  })
}
