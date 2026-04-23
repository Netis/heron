import { useState } from "react"
import { Info, X } from "lucide-react"
import { MetadataPopover } from "./metadata-popover"
import type { AgentTurnDetail } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  onClose: () => void
}

function truncateMid(s: string, head = 8, tail = 6): string {
  return s.length > head + tail + 1 ? `${s.slice(0, head)}…${s.slice(-tail)}` : s
}

export function TopBar({ turn, onClose }: Props) {
  const [metaOpen, setMetaOpen] = useState(false)
  return (
    <div className="relative flex h-10 shrink-0 items-center justify-between border-b border-border px-4">
      <h2 className="text-sm font-semibold">Agent Turn Detail</h2>
      <div className="flex items-center gap-3 text-xs text-muted-foreground">
        <span>{turn.agent_kind}</span>
        <span>·</span>
        <span className="font-mono" title={turn.turn_id}>{truncateMid(turn.turn_id)}</span>
        <button
          onClick={() => setMetaOpen((o) => !o)}
          className="rounded p-1 hover:bg-muted hover:text-foreground"
          aria-label="Show metadata"
        >
          <Info className="size-4" />
        </button>
        <button onClick={onClose} className="rounded p-1 hover:bg-muted hover:text-foreground">
          <X className="size-4" />
        </button>
      </div>
      {metaOpen && <MetadataPopover turn={turn} onClose={() => setMetaOpen(false)} />}
    </div>
  )
}
