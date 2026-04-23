import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { formatDateTimeMs, formatTime } from "@/lib/format"
import { TurnMetadataStrip } from "./turn-metadata-strip"
import type { SessionTurnItem } from "@/types/api"

const PREVIEW_CHARS = 120

function preview(text: string | null): string {
  if (!text) return ""
  const trimmed = text.trim().split("\n")[0] ?? ""
  return trimmed.length > PREVIEW_CHARS ? trimmed.slice(0, PREVIEW_CHARS) + "…" : trimmed
}

export function TurnBlock({
  turn,
  expanded,
  onToggle,
  onInspect,
}: {
  turn: SessionTurnItem
  expanded: boolean
  onToggle: () => void
  onInspect: (turnId: string) => void
}) {
  const hasFinalAnswer = turn.final_answer != null && turn.final_answer.length > 0

  return (
    <div className="mb-4">
      {/* USER */}
      <div className="flex items-start gap-3">
        <div className="w-[56px] shrink-0 pt-1 text-right text-xs text-muted-foreground">
          {formatTime(turn.start_time)}
        </div>
        <div className="flex-1 rounded-r border-l-2 border-blue-400 bg-blue-50/60 px-3 py-2 dark:border-blue-500 dark:bg-blue-950/30">
          <div className="text-[10px] font-semibold uppercase tracking-wide text-blue-600 dark:text-blue-300">
            👤 User{expanded ? ` · ${formatDateTimeMs(turn.start_time)}` : ""}
          </div>
          <div className={cn("mt-1 text-sm text-foreground", !expanded && "truncate")}>
            {expanded ? <Markdown text={turn.user_input ?? ""} /> : preview(turn.user_input)}
          </div>
        </div>
      </div>

      {/* ASSISTANT */}
      <div className="mt-1 flex items-start gap-3">
        <div className="w-[56px] shrink-0" />
        <div
          className={cn(
            "flex-1 rounded-r border-l-2 px-3 py-2",
            hasFinalAnswer
              ? "border-emerald-400 bg-emerald-50/60 dark:border-emerald-500 dark:bg-emerald-950/30"
              : "border-red-400 bg-red-50/60 dark:border-red-500 dark:bg-red-950/30",
          )}
        >
          <div
            className={cn(
              "text-[10px] font-semibold uppercase tracking-wide",
              hasFinalAnswer
                ? "text-emerald-700 dark:text-emerald-300"
                : "text-red-700 dark:text-red-300",
            )}
          >
            🎯 Assistant{!hasFinalAnswer ? " · incomplete" : ""}
          </div>
          <div
            className={cn(
              "mt-1 text-sm",
              !hasFinalAnswer && "italic text-muted-foreground",
              !expanded && "truncate",
            )}
          >
            {hasFinalAnswer ? (
              expanded ? (
                <Markdown text={turn.final_answer ?? ""} />
              ) : (
                preview(turn.final_answer)
              )
            ) : (
              "Turn ended without a final answer"
            )}
          </div>
        </div>
      </div>

      <TurnMetadataStrip turn={turn} expanded={expanded} onToggle={onToggle} onInspect={onInspect} />
    </div>
  )
}
