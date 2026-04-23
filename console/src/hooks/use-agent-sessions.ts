import { useInfiniteQuery, useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type {
  SessionsPage,
  SessionDetail,
  SessionTurnsPage,
} from "@/types/api"

interface UseAgentSessionsParams {
  sourceId?: string
  /** CSV of agent kinds, e.g. "claude-cli,codex-cli" */
  agentKind?: string
  pageSize?: number
}

const DEFAULT_PAGE_SIZE = 50

export function useAgentSessions({ sourceId, agentKind, pageSize = DEFAULT_PAGE_SIZE }: UseAgentSessionsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)

  return useInfiniteQuery({
    queryKey: ["agent-sessions", { start, end, sourceId, agentKind, pageSize }],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) =>
      apiFetch<SessionsPage>("/api/agent-sessions", {
        start,
        end,
        page_size: pageSize,
        source_id: sourceId || undefined,
        agent_kind: agentKind || undefined,
        cursor: pageParam || undefined,
      }),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  })
}

export function useAgentSessionDetail(sourceId: string | null, sessionId: string | null) {
  return useQuery({
    queryKey: ["agent-session-detail", sourceId, sessionId],
    queryFn: () =>
      apiFetch<SessionDetail>(
        `/api/agent-sessions/${encodeURIComponent(sourceId!)}/${encodeURIComponent(sessionId!)}`,
      ),
    enabled: sourceId != null && sessionId != null,
  })
}

export function useSessionTurns(
  sourceId: string | null,
  sessionId: string | null,
  pageSize = DEFAULT_PAGE_SIZE,
) {
  return useInfiniteQuery({
    queryKey: ["session-turns", sourceId, sessionId, pageSize],
    enabled: sourceId != null && sessionId != null,
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) =>
      apiFetch<SessionTurnsPage>(
        `/api/agent-sessions/${encodeURIComponent(sourceId!)}/${encodeURIComponent(sessionId!)}/turns`,
        {
          page_size: pageSize,
          cursor: pageParam || undefined,
        },
      ),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  })
}
