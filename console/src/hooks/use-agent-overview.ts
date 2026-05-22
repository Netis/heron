import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { AgentActivityData, AgentSummaryData } from "@/types/api"

export function useAgentSummary() {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  return useQuery({
    queryKey: ["agent-summary", { start, end }],
    queryFn: () =>
      apiFetch<AgentSummaryData>("/api/agent-turns/summary", { start, end }),
    placeholderData: (prev) => prev,
  })
}

export function useAgentActivity() {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)
  return useQuery({
    queryKey: ["agent-activity", { start, end }],
    queryFn: () =>
      apiFetch<AgentActivityData>("/api/agent-turns/activity", { start, end }),
    placeholderData: (prev) => prev,
  })
}
