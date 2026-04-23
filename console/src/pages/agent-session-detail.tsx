import { useCallback, useState } from "react"
import { Link, useParams } from "react-router"
import { ArrowLeft, Loader2 } from "lucide-react"
import { useAgentSessionDetail, useSessionTurns } from "@/hooks/use-agent-sessions"
import { SessionHeader, TurnBlock } from "@/components/session-detail"
import { AgentTurnDetailPanel } from "@/pages/agent-turn-detail-panel"

export function AgentSessionDetailPage() {
  const { source_id = "", session_id = "" } = useParams()
  const { data: detail, isLoading: loadingDetail, isError: errorDetail } =
    useAgentSessionDetail(source_id, session_id)
  const {
    data: turnsData,
    isLoading: loadingTurns,
    isError: errorTurns,
    fetchNextPage,
    hasNextPage,
    isFetchingNextPage,
  } = useSessionTurns(source_id, session_id)

  const [expandedTurns, setExpandedTurns] = useState<Set<string>>(new Set())
  const [selectedTurnId, setSelectedTurnId] = useState<string | null>(null)

  const toggleTurn = useCallback((turnId: string) => {
    setExpandedTurns((prev) => {
      const next = new Set(prev)
      if (next.has(turnId)) next.delete(turnId)
      else next.add(turnId)
      return next
    })
  }, [])

  if (loadingDetail && !detail) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-5 animate-spin text-muted-foreground" />
      </div>
    )
  }
  if (errorDetail || !detail) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 text-sm text-destructive">
        <span>Session not found</span>
        <Link
          to="/agent-sessions"
          className="rounded border border-border px-3 py-1 text-muted-foreground hover:bg-muted"
        >
          Back to sessions
        </Link>
      </div>
    )
  }

  const turns = turnsData?.pages.flatMap((p) => p.items) ?? []

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="shrink-0 border-b border-border px-4 py-3">
        <Link
          to="/agent-sessions"
          className="mb-2 inline-flex items-center gap-1 text-xs text-primary hover:underline"
        >
          <ArrowLeft className="size-3" /> Agent Sessions
        </Link>
        <SessionHeader detail={detail} />
      </div>

      <div className="flex-1 overflow-auto px-4 py-4">
        {loadingTurns && turns.length === 0 ? (
          <div className="flex h-40 items-center justify-center">
            <Loader2 className="size-5 animate-spin text-muted-foreground" />
          </div>
        ) : errorTurns ? (
          <div className="py-10 text-center text-sm text-destructive">Failed to load turns</div>
        ) : turns.length === 0 ? (
          <div className="py-10 text-center text-sm text-muted-foreground">No turns in this session</div>
        ) : (
          turns.map((t) => (
            <TurnBlock
              key={t.turn_id}
              turn={t}
              expanded={expandedTurns.has(t.turn_id)}
              onToggle={() => toggleTurn(t.turn_id)}
              onInspect={(id) => setSelectedTurnId(id)}
            />
          ))
        )}

        {hasNextPage && (
          <div className="pt-4 text-center">
            <button
              onClick={() => fetchNextPage()}
              disabled={isFetchingNextPage}
              className="rounded border border-border bg-background px-4 py-1.5 text-sm text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              {isFetchingNextPage ? "Loading…" : "Load older turns"}
            </button>
          </div>
        )}
      </div>

      {selectedTurnId && (
        <AgentTurnDetailPanel
          id={selectedTurnId}
          onClose={() => setSelectedTurnId(null)}
        />
      )}
    </div>
  )
}
