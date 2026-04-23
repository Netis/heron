import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { formatDuration, formatNumber } from "@/lib/format"
import type { SessionTurnItem } from "@/types/api"

export function TurnMetadataStrip({
  turn,
  onInspect,
}: {
  turn: SessionTurnItem
  onInspect?: (turnId: string) => void
}) {
  const tokensIn = formatNumber(turn.total_input_tokens)
  const tokensOut = formatNumber(turn.total_output_tokens)

  return (
    <div className="ml-[76px] flex items-center gap-2 px-2 py-1 text-xs text-muted-foreground">
      <TurnStatusBadge status={turn.status} />
      <span>
        {formatDuration(turn.duration_ms)} · {turn.call_count} calls · {tokensIn} in / {tokensOut} out
      </span>
      <span className="flex-1" />
      {onInspect && (
        <button
          onClick={() => onInspect(turn.turn_id)}
          className="text-primary hover:underline"
        >
          View turn detail →
        </button>
      )}
    </div>
  )
}
