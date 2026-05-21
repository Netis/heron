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
  /** CSV of u16 server ports e.g. "4210,9000" */
  serverPort?: string
  requestPath?: string
  /** Stream-mode filter: "stream", "non-stream", or undefined for all. */
  isStream?: string
}

export function useLlmCalls({
  page,
  pageSize,
  sortBy,
  sortOrder,
  statusCode,
  finishReason,
  clientIp,
  serverPort,
  requestPath,
  isStream,
}: UseLlmCallsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const { params: fp } = useSupportedFilterParams()

  return useQuery({
    queryKey: ["llm-calls", {
      start, end, page, pageSize, sortBy, sortOrder,
      ...fp,
      statusCode, finishReason, clientIp, serverPort, requestPath, isStream,
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
        server_port: serverPort || undefined,
        request_path: requestPath || undefined,
        is_stream: isStream || undefined,
      }),
    placeholderData: (prev) => prev,
  })
}
