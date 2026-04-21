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
  agentKind?: string
}

export function useTurns({ page, pageSize, sortBy, sortOrder, status, agentKind }: UseTurnsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const filters = useToolbarStore((s) => s.filters)

  return useQuery({
    queryKey: ["turns", {
      start, end, page, pageSize, sortBy, sortOrder,
      wireApi: filters.wireApi, model: filters.model, serverIp: filters.serverIp,
      status, agentKind,
    }],
    queryFn: () =>
      apiFetch<TurnsPage>("/api/turns", {
        start,
        end,
        page,
        page_size: pageSize,
        sort_by: sortBy,
        sort_order: sortOrder,
        wire_api: filters.wireApi || undefined,
        model: filters.model || undefined,
        server_ip: filters.serverIp || undefined,
        status: status || undefined,
        agent_kind: agentKind || undefined,
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
