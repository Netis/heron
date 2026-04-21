import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { AgentTurnsPage, AgentTurnDetail, AgentTurnCallItem } from "@/types/api"

interface UseAgentTurnsParams {
  page: number
  pageSize: number
  sortBy: string
  sortOrder: "asc" | "desc"
  /** CSV of statuses e.g. "success,error" */
  status?: string
  /** CSV of client kinds */
  agentKind?: string
}

export function useAgentTurns({ page, pageSize, sortBy, sortOrder, status, agentKind }: UseAgentTurnsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const filters = useToolbarStore((s) => s.filters)

  return useQuery({
    queryKey: ["agent-turns", {
      start, end, page, pageSize, sortBy, sortOrder,
      wireApi: filters.wireApi, model: filters.model, serverIp: filters.serverIp,
      status, agentKind,
    }],
    queryFn: () =>
      apiFetch<AgentTurnsPage>("/api/agent-turns", {
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

export function useAgentTurnDetail(id: string | null) {
  return useQuery({
    queryKey: ["agent-turn-detail", id],
    queryFn: () => apiFetch<AgentTurnDetail>(`/api/agent-turns/${id}`),
    enabled: id != null,
  })
}

export function useAgentTurnCalls(id: string | null) {
  return useQuery({
    queryKey: ["agent-turn-calls", id],
    queryFn: () => apiFetch<AgentTurnCallItem[]>(`/api/agent-turns/${id}/calls`),
    enabled: id != null,
  })
}
