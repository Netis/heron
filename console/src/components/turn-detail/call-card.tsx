import { useState } from "react"
import { ChevronRight, ChevronDown, Wrench, MessageSquare, Target } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import type { AgentTurnCallItem } from "@/types/api"

const SLOW_THRESHOLD_MS = 10_000

function TypeChip({ call }: { call: AgentTurnCallItem }) {
  if (call.type === "tool_call") {
    const names = call.tool_calls.slice(0, 2).map(t => t.name)
    const more = call.tool_calls.length - names.length
    return (
      <span className="flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-300">
        <Wrench className="size-3" />
        {names.join(", ")}
        {more > 0 && <span className="ml-1 opacity-70">+{more}</span>}
      </span>
    )
  }
  if (call.type === "final") {
    return (
      <span className="flex items-center gap-1 rounded bg-emerald-100 px-1.5 py-0.5 text-[10px] font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300">
        <Target className="size-3" /> final
      </span>
    )
  }
  return (
    <span className="flex items-center gap-1 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
      <MessageSquare className="size-3" /> text
    </span>
  )
}

function classify(call: AgentTurnCallItem): "normal" | "slow" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}

interface Props {
  call: AgentTurnCallItem
  active?: boolean
  defaultExpanded?: boolean
  onOpenRawHttp?: (id: string) => void
}

export function CallCard({ call, active, defaultExpanded, onOpenRawHttp }: Props) {
  const [expanded, setExpanded] = useState(Boolean(defaultExpanded))
  const speed = classify(call)
  return (
    <div
      id={`call-${call.sequence}`}
      className={cn(
        "rounded-lg border bg-background transition-colors",
        speed === "slow" && "border-l-2 border-l-amber-500/70 border-border",
        speed === "error" && "border-l-2 border-l-red-500/70 border-border",
        speed === "normal" && "border-border",
        active && "ring-2 ring-blue-400 ring-offset-1",
      )}
    >
      <button
        onClick={() => setExpanded((e) => !e)}
        className="w-full text-left"
      >
        <div className="flex w-full flex-col gap-1 px-3 py-2 text-left">
          <div className="flex items-center gap-3">
            <span className="w-6 shrink-0 tabular-nums text-xs text-muted-foreground">#{call.sequence}</span>
            <TypeChip call={call} />
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
          {(call.message_preview ?? call.tool_calls[0]?.args_preview) && (
            <div className="truncate pl-9 text-[11px] text-muted-foreground">
              {call.message_preview
                ? `"${call.message_preview}${(call.message_preview?.length ?? 0) >= 60 ? "…" : ""}"`
                : call.tool_calls[0].args_preview}
            </div>
          )}
        </div>
      </button>
      {expanded && (
        <div className="border-t border-border px-3 py-2 text-xs text-muted-foreground">
          <div>{call.wire_api} · TTFB {formatMs(call.ttfb_ms)} · finish: {call.finish_reason ?? "—"}</div>
          <button
            onClick={() => onOpenRawHttp?.(call.id)}
            className="mt-2 text-foreground hover:underline"
          >
            View raw HTTP →
          </button>
        </div>
      )}
    </div>
  )
}
