import { useEffect } from "react"
import { Loader2 } from "lucide-react"
import { useAgentTurnDetail, useAgentTurnCalls } from "@/hooks/use-agent-turns"
import { useTurnUrlState } from "@/hooks/use-turn-url-state"
import { RawHttpDrawer, type RawHttpData } from "@/components/turn-detail/raw-http-drawer"
import { TopBar, StatsCards, GanttNav, UserCard, FinalAnswerCard, CallCard } from "@/components/turn-detail"
import type { AgentTurnDetail, AgentTurnCallItem } from "@/types/api"

function toRawHttpData(call: AgentTurnCallItem): RawHttpData {
  return {
    id: call.id,
    wire_api: call.wire_api,
    model: call.model,
    status_code: call.status_code,
    finish_reason: call.finish_reason,
    ttft_ms: call.ttft_ms,
    e2e_latency_ms: call.e2e_latency_ms,
    input_tokens: call.input_tokens,
    output_tokens: call.output_tokens,
    request_path: call.request_path,
    client_ip: call.client_ip,
    client_port: call.client_port,
    server_ip: call.server_ip,
    server_port: call.server_port,
    is_stream: call.is_stream,
    request_time: call.request_time,
    request_body: call.request_body,
    response_body: call.response_body,
    request_headers: call.request_headers,
    response_headers: call.response_headers,
  }
}

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
            calls.map((c, i) => (
              <CallCard
                key={c.id}
                call={c}
                nextCall={calls[i + 1] ?? null}
                finalCallId={turn.final_call_id}
                agentKind={turn.agent_kind ?? null}
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

  const rawHttpCall = urlRaw && activeSeq != null
    ? calls.find((c) => c.sequence === activeSeq) ?? null
    : null
  const rawHttpData = rawHttpCall ? toRawHttpData(rawHttpCall) : null

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
        if (rawHttpData) { closeRawHttp(); return }
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
  }, [activeSeq, calls.length, rawHttpData])

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
                  activeSeq={activeSeq}
                  onSelect={handleSelect}
                  onOpenRawHttp={openRawHttp}
                />
              </div>
            </section>
          </div>
        )}

        <RawHttpDrawer data={rawHttpData} onClose={closeRawHttp} />
      </div>
    </>
  )
}
