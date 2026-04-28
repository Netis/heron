import { cn } from "@/lib/utils"
import { finishTone, TONE_CLASS } from "@/lib/finish-tone"

export function FinishBadge({ reason }: { reason: string | null }) {
  if (!reason) return <span className="text-muted-foreground">—</span>
  const tone = finishTone(reason)
  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-xs font-medium",
        TONE_CLASS[tone],
      )}
    >
      {reason}
    </span>
  )
}
