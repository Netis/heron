import { useMemo } from "react"
import { formatDuration, formatMs, formatNumber } from "@/lib/format"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"
import { cn } from "@/lib/utils"

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  onJumpToSlowest?: (sequence: number) => void
}

function Card({ label, children, className }: {
  label: string
  children: React.ReactNode
  className?: string
}) {
  return (
    <div className={cn("flex flex-col gap-0.5 rounded-lg border border-border bg-muted/30 px-3 py-2", className)}>
      <span className="text-xs text-muted-foreground">{label}</span>
      {children}
    </div>
  )
}

export function StatsCards({ turn, calls, onJumpToSlowest }: Props) {
  const slowest = useMemo(() => {
    let best: AgentTurnCallItem | null = null
    for (const c of calls) {
      if (c.e2e_latency_ms == null) continue
      if (!best || (c.e2e_latency_ms > (best.e2e_latency_ms ?? 0))) best = c
    }
    return best
  }, [calls])

  return (
    <div className="grid grid-cols-4 gap-3">
      <Card label="Calls">
        <div className="text-sm font-medium tabular-nums">{turn.call_count}</div>
        {/* Phase 2 will add: type breakdown row here */}
      </Card>
      <Card label="Tokens">
        <div className="flex items-center gap-3 text-sm font-medium tabular-nums">
          <span className="flex flex-col"><span className="text-[10px] text-muted-foreground">in</span><span>{formatNumber(turn.total_input_tokens)}</span></span>
          <span className="flex flex-col"><span className="text-[10px] text-muted-foreground">out</span><span>{formatNumber(turn.total_output_tokens)}</span></span>
        </div>
        {turn.total_cost_usd != null && (
          <div className="text-xs text-muted-foreground tabular-nums">${turn.total_cost_usd.toFixed(2)}</div>
        )}
      </Card>
      <Card label="Duration">
        <div className="text-sm font-medium tabular-nums">{formatDuration(turn.duration_ms)}</div>
        {slowest && (
          <button
            onClick={() => onJumpToSlowest?.(slowest!.sequence)}
            className="text-left text-xs text-muted-foreground hover:text-foreground tabular-nums"
          >
            slowest #{slowest.sequence} {formatMs(slowest.e2e_latency_ms)}
          </button>
        )}
      </Card>
      <Card label="Status / Finish">
        <div className="flex items-center gap-2">
          <TurnStatusBadge status={turn.status} />
          <FinishBadge reason={turn.final_finish_reason} />
        </div>
      </Card>
    </div>
  )
}
