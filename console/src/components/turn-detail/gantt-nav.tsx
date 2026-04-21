import { useMemo } from "react"
import { Wrench, MessageSquare, Target } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatDuration, formatMs } from "@/lib/format"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  activeSequence: number | null
  onSelect: (sequence: number) => void
}

const SLOW_THRESHOLD_MS = 10_000

function classifySpeed(call: AgentTurnCallItem): "normal" | "slow" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}

function TypeIcon({ call }: { call: AgentTurnCallItem }) {
  const cls = "size-3"
  if (call.type === "tool_call") return <Wrench className={cn(cls, "text-amber-600")} />
  if (call.type === "final")     return <Target className={cn(cls, "text-emerald-600")} />
  return <MessageSquare className={cn(cls, "text-blue-600")} />
}

export function GanttNav({ turn, calls, activeSequence, onSelect }: Props) {
  const { minStart, total } = useMemo(() => {
    if (calls.length === 0) return { minStart: turn.start_time, total: turn.duration_ms || 1 }
    const min = Math.min(...calls.map((c) => c.request_time))
    const max = Math.max(...calls.map((c) => c.complete_time ?? c.response_time ?? c.request_time))
    return { minStart: min, total: Math.max(max - min, 1) }
  }, [calls, turn])

  return (
    <aside className="flex w-[140px] shrink-0 flex-col border-r border-border">
      <div className="shrink-0 border-b border-border px-3 py-2">
        <div className="text-xs font-medium">Timeline</div>
        <div className="text-[11px] tabular-nums text-muted-foreground">{formatDuration(turn.duration_ms)}</div>
      </div>
      <div className="flex-1 overflow-y-auto p-1">
        {calls.length === 0 ? (
          <div className="flex h-20 items-center justify-center text-xs text-muted-foreground">No calls</div>
        ) : (
          calls.map((c) => {
            const end = c.complete_time ?? c.response_time ?? c.request_time
            const offset = ((c.request_time - minStart) / total) * 100
            const width = Math.max(((end - c.request_time) / total) * 100, 0.5)
            const speed = classifySpeed(c)
            return (
              <button
                key={c.id}
                onClick={() => onSelect(c.sequence)}
                className={cn(
                  "grid w-full grid-cols-[16px_16px_1fr_36px] items-center gap-1 rounded px-1 py-1 text-left text-[10px]",
                  activeSequence === c.sequence ? "bg-blue-50 dark:bg-blue-950/40" : "hover:bg-muted/60",
                  speed === "slow" && "border-l-2 border-amber-500/70",
                  speed === "error" && "border-l-2 border-red-500/70",
                )}
              >
                <span className="tabular-nums text-muted-foreground">{c.sequence}</span>
                <TypeIcon call={c} />
                <div className="relative h-2 rounded bg-muted">
                  <div
                    className={cn(
                      "absolute top-0 h-full rounded",
                      speed === "slow" && "bg-amber-500/80",
                      speed === "error" && "bg-red-500/80",
                      speed === "normal" && "bg-blue-400",
                    )}
                    style={{ left: `${offset}%`, width: `${width}%`, minWidth: "2px" }}
                  />
                </div>
                <span className={cn(
                  "text-right tabular-nums",
                  speed === "slow" && "text-amber-600",
                  speed === "error" && "text-red-600",
                  speed === "normal" && "text-muted-foreground",
                )}>
                  {formatMs(c.e2e_latency_ms)}
                </span>
              </button>
            )
          })
        )}
      </div>
    </aside>
  )
}
