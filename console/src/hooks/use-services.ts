import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { ServicesData } from "@/types/api"

interface UseServicesParams {
  sortBy?: string
  sortOrder?: "asc" | "desc"
  limit?: number
}

export function useServices({ sortBy = "call_count", sortOrder = "desc", limit = 200 }: UseServicesParams = {}) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)

  return useQuery({
    queryKey: ["services", { start, end, sortBy, sortOrder, limit }],
    queryFn: () =>
      apiFetch<ServicesData>("/api/services", {
        start,
        end,
        sort_by: sortBy,
        sort_order: sortOrder,
        limit,
      }),
    placeholderData: (prev) => prev,
  })
}
