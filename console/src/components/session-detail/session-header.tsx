import { AgentBadge } from "@/components/ui/agent-badge"
import { formatNumber, formatDuration } from "@/lib/format"
import type { SessionDetail } from "@/types/api"

export function SessionHeader({ detail }: { detail: SessionDetail }) {
  const cost = detail.total_cost_usd != null ? `$${detail.total_cost_usd.toFixed(2)}` : null
  const tokens = formatNumber(detail.total_input_tokens + detail.total_output_tokens)
  const duration = formatDuration(detail.last_turn_at - detail.first_turn_at)

  return (
    <div className="flex items-center gap-3 rounded-md border border-border bg-muted/30 px-3 py-2">
      <AgentBadge agentKind={detail.agent_kind} />
      <span className="font-mono text-xs text-muted-foreground">{detail.session_id}</span>
      <span className="text-xs text-muted-foreground">source: {detail.source_id || "(default)"}</span>
      <span className="flex-1" />
      <span className="text-xs text-muted-foreground">
        {detail.turn_count} turns · {detail.call_count} calls · {tokens} tok
        {cost ? ` · ${cost}` : ""} · {duration}
      </span>
    </div>
  )
}
