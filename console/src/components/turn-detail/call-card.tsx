import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { FinishBadge } from "@/components/ui/finish-badge"
import type { AgentTurnCallItem } from "@/types/api"

const SLOW_THRESHOLD_MS = 10_000

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
        className="flex w-full items-center gap-3 px-3 py-2 text-left"
      >
        <span className="w-6 shrink-0 tabular-nums text-xs text-muted-foreground">#{call.sequence}</span>
        {/* Phase 2 renders real type icons + tool chips here. */}
        <FinishBadge reason={call.finish_reason} />
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
