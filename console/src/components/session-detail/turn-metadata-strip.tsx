import { ChevronDown, ChevronUp } from "lucide-react"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { formatDuration, formatNumber } from "@/lib/format"
import type { SessionTurnItem } from "@/types/api"

export function TurnMetadataStrip({
  turn,
  expanded,
  onToggle,
  onInspect,
}: {
  turn: SessionTurnItem
  expanded: boolean
  onToggle: () => void
  onInspect?: (turnId: string) => void
}) {
  const tokensIn = formatNumber(turn.total_input_tokens)
  const tokensOut = formatNumber(turn.total_output_tokens)

  return (
    <div
      className="ml-[60px] flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-muted/50 hover:text-foreground"
      onClick={onToggle}
    >
      {expanded ? <ChevronUp className="size-3" /> : <ChevronDown className="size-3" />}
      <TurnStatusBadge status={turn.status} />
      <span>
        {formatDuration(turn.duration_ms)} · {turn.call_count} calls · {tokensIn} in / {tokensOut} out
      </span>
      <span className="flex-1" />
      {expanded && onInspect && (
        <button
          onClick={(e) => {
            e.stopPropagation()
            onInspect(turn.turn_id)
          }}
          className="text-primary hover:underline"
        >
          View turn detail →
        </button>
      )}
    </div>
  )
}
