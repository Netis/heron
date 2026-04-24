import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { Markdown } from "@/components/ui/markdown"
import { CallOutputDispatch, CallInputDispatch } from "@/components/call-renderers/dispatch"
import { CallChipDispatch } from "@/components/call-renderers/chips/dispatch"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"
import type { ToolIndex } from "@/lib/turn-index"

const SLOW_THRESHOLD_MS = 10_000

function classify(call: AgentTurnCallItem): "normal" | "slow" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}

interface Props {
  call: AgentTurnCallItem
  turn: AgentTurnDetail
  toolIndex: ToolIndex
  isFirstCall: boolean
  active?: boolean
  defaultExpanded?: boolean
  onOpenDetail?: (id: string) => void
}

export function CallCard({
  call,
  turn,
  toolIndex,
  isFirstCall,
  active,
  defaultExpanded,
  onOpenDetail,
}: Props) {
  const [expanded, setExpanded] = useState(Boolean(defaultExpanded))
  const speed = classify(call)
  const isFinalCall = call.id === turn.final_call_id
  const userInput = isFirstCall ? turn.user_input : null

  return (
    <div
      id={`call-${call.sequence}`}
      className={cn(
        "rounded-lg border bg-background transition-colors",
        speed === "slow" && "border-l-2 border-l-amber-500/70 border-border",
        speed === "error" && "border-l-2 border-l-red-500/70 border-border",
        isFinalCall && speed === "normal" && "border-l-2 border-l-emerald-500/70 border-border",
        speed === "normal" && !isFinalCall && "border-border",
        active && "ring-2 ring-blue-400 ring-offset-1",
      )}
    >
      <button onClick={() => setExpanded((e) => !e)} className="w-full text-left">
        <div className="flex w-full items-center gap-3 px-3 py-2 text-left">
          <span className="w-6 shrink-0 tabular-nums text-xs text-muted-foreground">#{call.sequence}</span>
          {isFirstCall && (
            <span className="shrink-0 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
              👤 user
            </span>
          )}
          <CallChipDispatch
            wireApi={call.wire_api}
            callId={call.id}
            responseBody={call.response_body}
            finalCallId={turn.final_call_id}
          />
          <span className="flex-1 truncate text-xs text-muted-foreground">{call.model}</span>
          <span className={cn(
            "shrink-0 text-xs tabular-nums",
            speed === "slow" && "text-amber-600",
            speed === "error" && "text-red-600",
            speed === "normal" && "text-muted-foreground",
          )}>
            {speed === "error" && "✗ "}{formatMs(call.e2e_latency_ms)}
          </span>
          <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
            {formatNumber(call.input_tokens)}↑ {formatNumber(call.output_tokens)}↓
          </span>
          {expanded ? <ChevronDown className="size-4 text-muted-foreground" /> : <ChevronRight className="size-4 text-muted-foreground" />}
        </div>
      </button>
      {expanded && (
        <div className="border-t border-border bg-muted/30 px-3 py-3 space-y-3">
          {/* Input subsection */}
          <section className="border-l-2 border-muted-foreground/30 pl-3">
            <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
              Input · request body
            </div>
            {userInput != null ? (
              <div className="rounded-lg border border-blue-200 bg-blue-50/60 p-3 dark:border-blue-900/40 dark:bg-blue-900/10">
                <Markdown text={userInput} />
              </div>
            ) : (
              <CallInputDispatch
                wireApi={call.wire_api}
                agentKind={turn.agent_kind ?? null}
                requestBody={call.request_body}
                toolIndex={toolIndex}
              />
            )}
          </section>

          {/* Output subsection */}
          <section className="border-l-2 border-emerald-500/40 pl-3">
            <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
              Output · response body
            </div>
            <CallOutputDispatch
              wireApi={call.wire_api}
              agentKind={turn.agent_kind ?? null}
              responseBody={call.response_body}
              toolIndex={toolIndex}
              callId={call.id}
            />
          </section>

          <div className="text-[10px] text-muted-foreground font-mono">
            {call.model} · {call.wire_api} · TTFB {formatMs(call.ttft_ms)} · finish: {call.finish_reason ?? "—"}
          </div>
          <button onClick={() => onOpenDetail?.(call.id)} className="text-xs text-foreground hover:underline">
            View call detail →
          </button>
        </div>
      )}
    </div>
  )
}
