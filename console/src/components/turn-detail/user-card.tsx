import { useState } from "react"
import { Markdown } from "@/components/ui/markdown"
import { formatDateTimeMs } from "@/lib/format"

interface Props {
  text: string
  startTime: number
}

export function UserCard({ text, startTime }: Props) {
  const [expanded, setExpanded] = useState(false)
  const long = text.split("\n").length > 8 || text.length > 600
  return (
    <div className="rounded-lg border border-blue-200 bg-blue-50/60 p-4 dark:border-blue-900 dark:bg-blue-950/30">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-sm font-medium">👤 User</span>
        <span className="text-xs tabular-nums text-muted-foreground">{formatDateTimeMs(startTime)}</span>
      </div>
      <div className={long && !expanded ? "max-h-[240px] overflow-hidden" : ""}>
        <Markdown text={text} />
      </div>
      {long && (
        <button
          onClick={() => setExpanded((e) => !e)}
          className="mt-2 text-xs text-muted-foreground hover:text-foreground"
        >
          {expanded ? "Show less ▴" : "Show more ▾"}
        </button>
      )}
    </div>
  )
}
