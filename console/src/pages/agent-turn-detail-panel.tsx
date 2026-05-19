import { useEffect, useMemo, useState } from "react"
import { Loader2, ArrowLeftRight, Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { useAgentTurnDetail, useAgentTurnCalls } from "@/hooks/use-agent-turns"
import { useTurnUrlState } from "@/hooks/use-turn-url-state"
import { LlmCallDetailPanel } from "./llm-call-detail-panel"
import { TopBar, StatsCards, GanttNav, CallCard } from "@/components/turn-detail"
import { ProxyViewTab } from "@/components/turn-detail/proxy-view-tab"
import { buildToolIndex } from "@/lib/turn-index"
import { groupCalls } from "@/lib/call-pair"
import type { AgentTurnDetail, AgentTurnCallItem } from "@/types/api"

type DetailTab = "calls" | "proxy"

/** Read `metadata.proxy.role` off a turn detail — present only when the
 * backend pair sweeper has classified this turn as part of a proxy
 * group. We surface the "Proxy View" tab solely on that condition.
 *
 * `AgentTurnDetail.metadata` is `unknown`-typed at the TS level (the
 * shape is open-ended JSON), so we walk it defensively. */
function readProxyRole(turn: AgentTurnDetail): string | null {
  const meta = turn.metadata
  if (!meta || typeof meta !== "object") return null
  const proxy = (meta as Record<string, unknown>).proxy
  if (!proxy || typeof proxy !== "object") return null
  const role = (proxy as Record<string, unknown>).role
  return typeof role === "string" ? role : null
}

interface Props {
  id: string
  onClose: () => void
}

function TurnDetailView({
  turn,
  calls,
  renderedCalls,
  loadingCalls,
  activeSeq,
  onSelect,
  onOpenDetail,
  foldHops,
  setFoldHops,
  hopsByCanonical,
  hopCount,
}: {
  turn: AgentTurnDetail
  /** Full call list — used for indexing tools etc. */
  calls: AgentTurnCallItem[]
  /** Calls to render in the list/timeline (full list, or canonical-only
   * when foldHops is on). Sibling GanttNav uses the same view. */
  renderedCalls: AgentTurnCallItem[]
  loadingCalls: boolean
  activeSeq: number | null
  onSelect: (seq: number) => void
  onOpenDetail: (id: string) => void
  foldHops: boolean
  setFoldHops: (v: boolean) => void
  hopsByCanonical: Map<string, AgentTurnCallItem[]>
  hopCount: number
}) {
  const toolIndex = useMemo(() => buildToolIndex(calls), [calls])
  const userCallId = turn.user_call_id ?? calls[0]?.id ?? null
  const proxyRole = readProxyRole(turn)
  const [tab, setTab] = useState<DetailTab>("calls")

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="shrink-0 p-4 pb-0">
        <StatsCards
          turn={turn}
          calls={calls}
          onJumpToSlowest={onSelect}
        />
      </div>
      <div className="flex shrink-0 items-center gap-2 border-b border-border px-4">
        {proxyRole && (
          <>
            <TabButton active={tab === "calls"} onClick={() => setTab("calls")}>
              Calls
            </TabButton>
            <TabButton active={tab === "proxy"} onClick={() => setTab("proxy")}>
              <ArrowLeftRight className="size-3" />
              Proxy view
            </TabButton>
          </>
        )}
        {tab === "calls" && hopCount > 0 && (
          <label
            className="ml-auto inline-flex cursor-pointer select-none items-center gap-1.5 py-2 text-xs text-muted-foreground hover:text-foreground"
            title={
              foldHops
                ? `${hopCount} duplicate call leg(s) folded — show them?`
                : "Hide proxy-duplicated legs and keep one row per logical call"
            }
          >
            <input
              type="checkbox"
              checked={!foldHops}
              onChange={(e) => setFoldHops(!e.target.checked)}
              className="size-3"
            />
            <Layers className="size-3" />
            Show proxy hops ({hopCount})
          </label>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {tab === "proxy" && proxyRole ? (
          <ProxyViewTab turnId={turn.turn_id} />
        ) : (
          <div className="flex flex-col gap-3 p-4">
            {loadingCalls && calls.length === 0 ? (
              <>
                {[0, 1, 2].map((i) => (
                  <div key={i} className="h-12 animate-pulse rounded-lg border border-border bg-muted/40" />
                ))}
              </>
            ) : (
              renderedCalls.map((c) => (
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
                  hopCount={hopsByCanonical.get(c.id)?.length ?? 0}
                />
              ))
            )}
            {!loadingCalls && renderedCalls.length === 0 && (
              <p className="text-center text-xs text-muted-foreground">No calls</p>
            )}
          </div>
        )}
      </div>
    </div>
  )
}

function TabButton({
  active,
  onClick,
  children,
}: {
  active: boolean
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex items-center gap-1.5 border-b-2 px-3 py-2 text-xs font-medium transition-colors",
        active
          ? "border-foreground text-foreground"
          : "border-transparent text-muted-foreground hover:text-foreground",
      )}
    >
      {children}
    </button>
  )
}

export function AgentTurnDetailPanel({ id, onClose }: Props) {
  const { data: turn, isLoading: loadingTurn, isError: errorTurn } = useAgentTurnDetail(id)
  const { data: calls = [], isLoading: loadingCalls } = useAgentTurnCalls(id)

  // Call-level proxy-duplicate fold: when two captured calls represent
  // the same LLM round-trip (e.g. client→litellm + litellm→upstream),
  // hide the upstream-facing leg by default. Toggle exposed in
  // TurnDetailView below the StatsCards. Shared between GanttNav and
  // the CallCard list so the timeline matches what's rendered to the
  // right.
  const [foldHops, setFoldHops] = useState(true)
  const callGrouping = useMemo(() => groupCalls(calls), [calls])
  const renderedCalls = foldHops ? callGrouping.visible : calls

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
            {/* Left panel — Gantt nav, filtered by the same fold rule */}
            <GanttNav turn={turn} calls={renderedCalls} activeSequence={activeSeq} onSelect={handleSelect} />

            {/* Right panel */}
            <section className="flex flex-1 flex-col overflow-hidden">
              <TopBar turn={turn} onClose={onClose} />

              <div className="flex min-h-0 flex-1 flex-col">
                <TurnDetailView
                  turn={turn}
                  calls={calls}
                  renderedCalls={renderedCalls}
                  loadingCalls={loadingCalls}
                  activeSeq={activeSeq}
                  onSelect={handleSelect}
                  onOpenDetail={openCallDetail}
                  foldHops={foldHops}
                  setFoldHops={setFoldHops}
                  hopsByCanonical={callGrouping.hopsByCanonical}
                  hopCount={callGrouping.hopCount}
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
