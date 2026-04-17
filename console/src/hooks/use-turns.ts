import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { TurnsPage, TurnDetail, TurnCallItem } from "@/types/api"

interface UseTurnsParams {
  page: number
  pageSize: number
  sortBy: string
  sortOrder: "asc" | "desc"
  /** CSV of statuses e.g. "success,error" */
  status?: string
  /** CSV of client kinds */
  clientKind?: string
}

export function useTurns({ page, pageSize, sortBy, sortOrder, status, clientKind }: UseTurnsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const filters = useToolbarStore((s) => s.filters)

  return useQuery({
    queryKey: ["turns", {
      start, end, page, pageSize, sortBy, sortOrder,
      provider: filters.provider, model: filters.model, serverIp: filters.serverIp,
      status, clientKind,
    }],
    queryFn: () =>
      apiFetch<TurnsPage>("/api/turns", {
        start,
        end,
        page,
        page_size: pageSize,
        sort_by: sortBy,
        sort_order: sortOrder,
        provider: filters.provider || undefined,
        model: filters.model || undefined,
        server_ip: filters.serverIp || undefined,
        status: status || undefined,
        client_kind: clientKind || undefined,
      }),
    placeholderData: (prev) => prev,
  })
}

export function useTurnDetail(id: string | null) {
  return useQuery({
    queryKey: ["turn-detail", id],
    queryFn: () => apiFetch<TurnDetail>(`/api/turns/${id}`),
    enabled: id != null,
  })
}

export function useTurnCalls(id: string | null) {
  return useQuery({
    queryKey: ["turn-calls", id],
    queryFn: () => apiFetch<TurnCallItem[]>(`/api/turns/${id}/calls`),
    enabled: id != null,
  })
}
