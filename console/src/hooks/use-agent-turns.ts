import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"
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
  const { params: fp } = useSupportedFilterParams()

  return useQuery({
    queryKey: ["agent-turns", {
      start, end, page, pageSize, sortBy, sortOrder,
      ...fp,
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
        ...fp,
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
