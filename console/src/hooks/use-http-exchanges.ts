import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"
import type { HttpExchangesPage } from "@/types/api"

interface UseHttpExchangesParams {
  page: number
  pageSize: number
  sortBy: string
  sortOrder: "asc" | "desc"
  /** CSV of HTTP methods; case-insensitive. */
  method?: string
  /** CSV of status codes. */
  status?: string
  /** Tri-state: true → SSE only, false → non-SSE only, undefined → any. */
  isSse?: boolean
}

export function useHttpExchanges({
  page,
  pageSize,
  sortBy,
  sortOrder,
  method,
  status,
  isSse,
}: UseHttpExchangesParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const { params: fp } = useSupportedFilterParams()

  return useQuery({
    queryKey: [
      "http-exchanges",
      {
        start,
        end,
        page,
        pageSize,
        sortBy,
        sortOrder,
        ...fp,
        method,
        status,
        isSse,
      },
    ],
    queryFn: () =>
      apiFetch<HttpExchangesPage>("/api/http-exchanges", {
        start,
        end,
        page,
        page_size: pageSize,
        sort_by: sortBy,
        sort_order: sortOrder,
        ...fp,
        method: method || undefined,
        status: status || undefined,
        is_sse: isSse === undefined ? undefined : String(isSse),
      }),
    placeholderData: (prev) => prev,
  })
}
