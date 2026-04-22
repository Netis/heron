import { useEffect } from "react"
import { Loader2 } from "lucide-react"
import { useAgentTurnDetail, useAgentTurnCalls } from "@/hooks/use-agent-turns"
import { useTurnUrlState } from "@/hooks/use-turn-url-state"
import { RawHttpDrawer } from "@/components/turn-detail/raw-http-drawer"
import { TopBar, StatsCards, GanttNav, UserCard, FinalAnswerCard, CallCard } from "@/components/turn-detail"
import type { AgentTurnDetail, AgentTurnCallItem } from "@/types/api"

interface Props {
  id: string
  onClose: () => void
}

function TurnDetailView({
  turn,
  calls,
  loadingCalls,
  activeSeq,
  onSelect,
  onOpenRawHttp,
}: {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  loadingCalls: boolean
  activeSeq: number | null
  onSelect: (seq: number) => void
  onOpenRawHttp: (id: string) => void
}) {
  const finalCall = calls.find((c) => c.id === turn.final_call_id) ?? calls[calls.length - 1]

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="shrink-0 p-4 pb-0">
        <StatsCards turn={turn} calls={calls} onJumpToSlowest={onSelect} />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        <div className="flex flex-col gap-3">
          {turn.user_input && <UserCard text={turn.user_input} startTime={turn.start_time} />}
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
                active={c.sequence === activeSeq}
                defaultExpanded={c.sequence === activeSeq}
                onOpenRawHttp={onOpenRawHttp}
              />
            ))
          )}
          {!loadingCalls && calls.length === 0 && (
            <p className="text-center text-xs text-muted-foreground">No calls</p>
          )}
          {turn.final_answer
            ? <FinalAnswerCard text={turn.final_answer} finalCall={finalCall} onJumpToCall={onSelect} />
            : calls.length > 0 && (
                <p className="text-center text-xs text-muted-foreground">Turn ended without a final answer</p>
              )}
        </div>
      </div>
    </div>
  )
}

export function AgentTurnDetailPanel({ id, onClose }: Props) {
  const { data: turn, isLoading: loadingTurn, isError: errorTurn } = useAgentTurnDetail(id)
  const { data: calls = [], isLoading: loadingCalls } = useAgentTurnCalls(id)

  const { call: activeSeq, raw: urlRaw, setCall, setRaw, openRaw } = useTurnUrlState()

  const rawHttpCallId = urlRaw && activeSeq != null
    ? calls.find((c) => c.sequence === activeSeq)?.id ?? null
    : null

  const handleSelect = (seq: number) => {
    setCall(seq)
    document.getElementById(`call-${seq}`)?.scrollIntoView({ behavior: "smooth", block: "start" })
  }

  const openRawHttp = (id: string) => {
    const call = calls.find((c) => c.id === id)
    if (call) openRaw(call.sequence)
  }

  const closeRawHttp = () => {
    setRaw(false)
  }

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (rawHttpCallId) { closeRawHttp(); return }
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
  }, [activeSeq, calls.length, rawHttpCallId])

  return (
    <>
      {/* Backdrop */}
      <div className="fixed inset-0 z-40 bg-black/20" onClick={onClose} />

      {/* Panel */}
      <div className="fixed top-0 right-0 z-50 flex h-full w-[92%] min-w-[720px] flex-col border-l border-border bg-background shadow-xl animate-in slide-in-from-right duration-200">
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
                  activeSeq={activeSeq}
                  onSelect={handleSelect}
                  onOpenRawHttp={openRawHttp}
                />
              </div>
            </section>
          </div>
        )}

        <RawHttpDrawer callId={rawHttpCallId} onClose={closeRawHttp} />
      </div>
    </>
  )
}
