import { X } from "lucide-react"
import { formatDateTimeMs } from "@/lib/format"
import type { AgentTurnDetail } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  onClose: () => void
}

export function MetadataPopover({ turn, onClose }: Props) {
  const rows: [string, string][] = [
    ["Trace ID", turn.turn_id],
    ["Source", turn.source_id || "—"],
    ["Session ID", turn.session_id],
    ["Agent", turn.agent_kind],
    ["Wire API", turn.wire_api],
    ["Start", formatDateTimeMs(turn.start_time)],
    ["End", formatDateTimeMs(turn.end_time)],
    ["Models", turn.models_used.join(", ") || "—"],
    ["Subagents", turn.subagents_used.join(", ") || "—"],
  ]
  return (
    <div className="absolute right-10 top-10 z-10 w-[420px] rounded-lg border border-border bg-background p-4 shadow-xl">
      <div className="mb-3 flex items-center justify-between">
        <h3 className="text-sm font-semibold">Metadata</h3>
        <button onClick={onClose} className="rounded p-1 hover:bg-muted">
          <X className="size-4" />
        </button>
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
        {rows.map(([k, v]) => (
          <div key={k} className="contents">
            <span className="text-muted-foreground">{k}</span>
            <span className="break-all font-mono text-xs" title={v}>{v}</span>
          </div>
        ))}
      </div>
    </div>
  )
}
