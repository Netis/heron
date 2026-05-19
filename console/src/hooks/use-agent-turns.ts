import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import { useSupportedFilterParams } from "@/hooks/use-supported-filters"
import type { AgentTurnsPage, AgentTurnDetail, AgentTurnCallItem, ProxyViewResponse } from "@/types/api"

interface UseAgentTurnsParams {
  page: number
  pageSize: number
  sortBy: string
  sortOrder: "asc" | "desc"
  /** CSV of statuses e.g. "success,error" */
  status?: string
  /** CSV of client kinds */
  agentKind?: string
  /** CSV of client IPs e.g. "10.0.0.1,10.0.0.2" */
  clientIp?: string
  /** When true, return turns the pair sweeper marked hidden
   * (`proxy_out` / `mirror_secondary`). Default false. */
  includeProxyHops?: boolean
}

export function useAgentTurns({ page, pageSize, sortBy, sortOrder, status, agentKind, clientIp, includeProxyHops }: UseAgentTurnsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  const { params: fp } = useSupportedFilterParams()

  return useQuery({
    queryKey: ["agent-turns", {
      start, end, page, pageSize, sortBy, sortOrder,
      ...fp,
      status, agentKind, clientIp, includeProxyHops,
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
        client_ip: clientIp || undefined,
        include_proxy_hops: includeProxyHops ? "true" : undefined,
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

/** Fetches the multi-leg fold for a turn that's part of a proxy group.
 * Returns a 404 (and a useQuery error) when the turn is not part of any
 * group — callers gate on `turn.proxy_role` being set before showing
 * the proxy-view tab. */
export function useAgentTurnProxyView(id: string | null, enabled = true) {
  return useQuery({
    queryKey: ["agent-turn-proxy-view", id],
    queryFn: () => apiFetch<ProxyViewResponse>(`/api/agent-turns/${id}/proxy-view`),
    enabled: id != null && enabled,
  })
}
