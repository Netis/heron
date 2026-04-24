import { useMemo } from "react"
import { Wrench, MessageSquare, Target, AlertTriangle } from "lucide-react"
import { formatDuration, formatMs, formatNumber } from "@/lib/format"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { classifyType } from "@/lib/wire-apis/dispatch"
import { countUnresolved, type ToolIndex } from "@/lib/turn-index"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"
import { cn } from "@/lib/utils"

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  toolIndex: ToolIndex
  onJumpToSlowest?: (sequence: number) => void
  onJumpToFirstAnomaly?: () => void
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

export function StatsCards({ turn, calls, toolIndex, onJumpToSlowest, onJumpToFirstAnomaly }: Props) {
  const slowest = useMemo(() => {
    let best: AgentTurnCallItem | null = null
    for (const c of calls) {
      if (c.e2e_latency_ms == null) continue
      if (!best || (c.e2e_latency_ms > (best.e2e_latency_ms ?? 0))) best = c
    }
    return best
  }, [calls])

  const typeCounts = useMemo(() => {
    const acc = { tool_call: 0, text: 0, final: 0 }
    for (const c of calls) {
      const t = classifyType(c.wire_api, c.response_body, c.id, turn.final_call_id)
      acc[t]++
    }
    return acc
  }, [calls, turn.final_call_id])

  const unresolved = useMemo(
    () => countUnresolved(toolIndex, { final_call_id: turn.final_call_id, final_finish_reason: turn.final_finish_reason }, turn.final_call_id),
    [toolIndex, turn.final_call_id, turn.final_finish_reason],
  )

  return (
    <div className="grid grid-cols-4 gap-3">
      <Card label="Calls">
        <div className="text-sm font-medium tabular-nums">{turn.call_count}</div>
        <div className="flex items-center gap-2 text-[10px] text-muted-foreground">
          <span className="inline-flex items-center gap-0.5"><Wrench className="size-2.5" />{typeCounts.tool_call}</span>
          <span className="inline-flex items-center gap-0.5"><MessageSquare className="size-2.5" />{typeCounts.text}</span>
          <span className="inline-flex items-center gap-0.5"><Target className="size-2.5" />{typeCounts.final}</span>
        </div>
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
      {unresolved > 0 ? (
        <button
          onClick={onJumpToFirstAnomaly}
          className="flex flex-col gap-0.5 rounded-lg border border-amber-200 bg-amber-50 px-3 py-2 text-left hover:bg-amber-100 dark:border-amber-900/40 dark:bg-amber-900/10 dark:hover:bg-amber-900/20"
        >
          <span className="flex items-center gap-1 text-xs text-amber-700 dark:text-amber-400">
            <AlertTriangle className="size-3" /> Unresolved
          </span>
          <span className="text-sm font-medium tabular-nums text-amber-800 dark:text-amber-300">{unresolved}</span>
          <span className="text-[10px] text-amber-700 dark:text-amber-400">possible capture gap</span>
        </button>
      ) : (
        <Card label="Status">
          <div><TurnStatusBadge status={turn.status} /></div>
        </Card>
      )}
    </div>
  )
}
