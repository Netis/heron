import { useEffect, useMemo } from "react"
import { Loader2 } from "lucide-react"
import { useAgentTurnDetail, useAgentTurnCalls } from "@/hooks/use-agent-turns"
import { useTurnUrlState } from "@/hooks/use-turn-url-state"
import { LlmCallDetailPanel } from "./llm-call-detail-panel"
import { TopBar, StatsCards, GanttNav, CallCard } from "@/components/turn-detail"
import { buildToolIndex } from "@/lib/turn-index"
import type { AgentTurnDetail, AgentTurnCallItem } from "@/types/api"

interface Props {
  id: string
  onClose: () => void
}

function TurnDetailView({
  turn,
  calls,
  loadingCalls,
  liteMode,
  activeSeq,
  onSelect,
  onOpenDetail,
}: {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  loadingCalls: boolean
  liteMode: boolean
  activeSeq: number | null
  onSelect: (seq: number) => void
  onOpenDetail: (id: string) => void
}) {
  const toolIndex = useMemo(() => buildToolIndex(calls), [calls])

  const userCallId = turn.user_call_id ?? calls[0]?.id ?? null

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="shrink-0 p-4 pb-0">
        <StatsCards
          turn={turn}
          calls={calls}
          onJumpToSlowest={onSelect}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        <div className="flex flex-col gap-3">
          {liteMode && (
            <div className="rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-900 dark:border-amber-700/60 dark:bg-amber-900/20 dark:text-amber-200">
              Large turn ({turn.call_count} calls) — request/response bodies
              omitted from the list. Expand any call to fetch its bodies on
              demand.
            </div>
          )}
          {loadingCalls && calls.length === 0 ? (
            <>
              {[0, 1, 2].map((i) => (
                <div key={i} className="h-12 animate-pulse rounded-lg border border-border bg-muted/40" />
              ))}
            </>
          ) : (
            calls.map((c) => (
              <CallCard
                key={c.id}
                call={c}
                turn={turn}
                toolIndex={toolIndex}
                isFirstCall={c.id === userCallId}
                active={c.sequence === activeSeq}
                defaultExpanded={
                  c.sequence === activeSeq ||
                  c.id === userCallId ||
                  c.id === turn.final_call_id
                }
                onOpenDetail={onOpenDetail}
              />
            ))
          )}
          {!loadingCalls && calls.length === 0 && (
            <p className="text-center text-xs text-muted-foreground">No calls</p>
          )}
        </div>
      </div>
    </div>
  )
}

/// Above this call_count threshold, the calls list switches to lite
/// mode — server NULLs the four heavy body/header fields so a
/// mega-turn (hundreds of agentic iterations × hundreds of KB
/// request_body each) doesn't OOM the browser. Individual call bodies
/// are still reachable per-card via `useLlmCallDetail`. Threshold is
/// empirical: a 100-call turn round-trips in well under a second; a
/// 300-call turn (~60 MB at p50 body size) starts dropping frames.
const CALLS_LITE_THRESHOLD = 200

export function AgentTurnDetailPanel({ id, onClose }: Props) {
  const { data: turn, isLoading: loadingTurn, isError: errorTurn } = useAgentTurnDetail(id)
  const liteMode = (turn?.call_count ?? 0) > CALLS_LITE_THRESHOLD
  const { data: calls = [], isLoading: loadingCalls } = useAgentTurnCalls(id, liteMode)

  const { call: activeSeq, detail, setCall, setDetail, openDetail } = useTurnUrlState()

  const activeCall = activeSeq != null
    ? calls.find((c) => c.sequence === activeSeq) ?? null
    : null
  const activeIndex = activeCall ? calls.findIndex((c) => c.sequence === activeCall.sequence) : -1

  const handleSelect = (seq: number) => {
    setCall(seq)
    document.getElementById(`call-${seq}`)?.scrollIntoView({ behavior: "smooth", block: "start" })
  }

  const openCallDetail = (callId: string) => {
    const call = calls.find((c) => c.id === callId)
    if (call) openDetail(call.sequence)
  }

  const closeCallDetail = () => {
    setDetail(false)
  }

  const navigateCallDetail = (direction: "prev" | "next") => {
    if (activeIndex < 0) return
    const nextCall = calls[direction === "prev" ? activeIndex - 1 : activeIndex + 1]
    if (nextCall) openDetail(nextCall.sequence)
  }

  const detailOpen = detail && activeCall != null

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (detailOpen) { closeCallDetail(); return }
        onClose()
        return
      }
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        if (calls.length === 0) return
        const delta = e.key === "ArrowDown" ? 1 : -1
        const cur = activeSeq ?? 0
        const nextSeq = Math.max(1, Math.min(calls.length, cur + delta))
        handleSelect(nextSeq)
        e.preventDefault()
      }
      if (e.key === "Enter" && activeSeq != null) {
        const el = document.getElementById(`call-${activeSeq}`)
        el?.querySelector("button")?.click()
        e.preventDefault()
      }
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeSeq, calls.length, detailOpen])

  return (
    <>
      {/* Backdrop */}
      <div className="fixed inset-0 z-40 bg-black/20" onClick={onClose} />

      {/* Panel */}
      <div className="fixed top-0 right-0 z-50 flex h-full w-[70%] min-w-[560px] flex-col border-l border-border bg-background shadow-xl animate-in slide-in-from-right duration-200">
        {loadingTurn && !turn ? (
          <div className="flex flex-1 items-center justify-center">
            <Loader2 className="size-5 animate-spin text-muted-foreground" />
          </div>
        ) : errorTurn || !turn ? (
          <div className="flex flex-1 flex-col items-center justify-center gap-4 text-destructive">
            <span>Failed to load agent turn detail</span>
            <button
              onClick={onClose}
              className="rounded border border-border px-3 py-1 text-sm text-muted-foreground hover:bg-muted"
            >
              Close
            </button>
          </div>
        ) : (
          <div className="flex flex-1 overflow-hidden">
            {/* Left panel — Gantt nav */}
            <GanttNav turn={turn} calls={calls} activeSequence={activeSeq} onSelect={handleSelect} />

            {/* Right panel */}
            <section className="flex flex-1 flex-col overflow-hidden">
              <TopBar turn={turn} onClose={onClose} />

              <div className="flex min-h-0 flex-1 flex-col">
                <TurnDetailView
                  turn={turn}
                  calls={calls}
                  loadingCalls={loadingCalls}
                  liteMode={liteMode}
                  activeSeq={activeSeq}
                  onSelect={handleSelect}
                  onOpenDetail={openCallDetail}
                />
              </div>
            </section>
          </div>
        )}

        {detailOpen && activeCall && (
          <LlmCallDetailPanel
            id={activeCall.id}
            onClose={closeCallDetail}
            onNavigate={navigateCallDetail}
            hasPrev={activeIndex > 0}
            hasNext={activeIndex >= 0 && activeIndex < calls.length - 1}
          />
        )}
      </div>
    </>
  )
}
