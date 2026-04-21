import { Markdown } from "@/components/ui/markdown"
import { formatMs } from "@/lib/format"
import type { AgentTurnCallItem } from "@/types/api"

interface Props {
  text: string
  finalCall?: AgentTurnCallItem
  onJumpToCall?: (sequence: number) => void
}

export function FinalAnswerCard({ text, finalCall, onJumpToCall }: Props) {
  return (
    <div className="rounded-lg border border-emerald-200 bg-emerald-50/60 p-4 dark:border-emerald-900 dark:bg-emerald-950/30">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-sm font-medium">🎯 Final Answer</span>
        {finalCall && (
          <button
            onClick={() => onJumpToCall?.(finalCall.sequence)}
            className="text-xs tabular-nums text-muted-foreground hover:text-foreground"
          >
            #{finalCall.sequence} · {formatMs(finalCall.e2e_latency_ms)}
          </button>
        )}
      </div>
      <Markdown text={text} />
    </div>
  )
}
