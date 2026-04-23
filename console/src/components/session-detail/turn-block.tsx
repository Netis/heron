import { useLayoutEffect, useRef, useState } from "react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { formatTime } from "@/lib/format"
import { TurnMetadataStrip } from "./turn-metadata-strip"
import type { SessionTurnItem } from "@/types/api"

function ClampedMarkdown({ text }: { text: string }) {
  const ref = useRef<HTMLDivElement>(null)
  const [expanded, setExpanded] = useState(false)
  const [truncated, setTruncated] = useState(false)

  useLayoutEffect(() => {
    if (expanded) return
    const el = ref.current
    if (!el) return
    const check = () => setTruncated(el.scrollHeight > el.clientHeight + 1)
    check()
    const ro = new ResizeObserver(check)
    ro.observe(el)
    return () => ro.disconnect()
  }, [text, expanded])

  return (
    <>
      <div ref={ref} className={cn("mt-1", !expanded && "line-clamp-3")}>
        <Markdown text={text} compact={!expanded} />
      </div>
      {!expanded && truncated && (
        <button
          type="button"
          onClick={() => setExpanded(true)}
          className="mt-1 text-[11px] text-muted-foreground hover:text-foreground"
        >
          … show more ↓
        </button>
      )}
      {expanded && (
        <button
          type="button"
          onClick={() => setExpanded(false)}
          className="mt-1 text-[11px] text-muted-foreground hover:text-foreground"
        >
          show less ↑
        </button>
      )}
    </>
  )
}

export function TurnBlock({
  turn,
  onInspect,
}: {
  turn: SessionTurnItem
  onInspect: (turnId: string) => void
}) {
  const hasFinalAnswer = turn.final_answer != null && turn.final_answer.length > 0

  return (
    <div className="mb-6">
      {/* USER — prompt-style: no fill, left-border only */}
      <div className="flex items-start gap-3">
        <div className="w-[64px] shrink-0 pt-1 text-right font-mono text-xs tabular-nums text-muted-foreground">
          {formatTime(turn.start_time).slice(0, 8)}
        </div>
        <div className="flex-1 border-l-2 border-border py-0.5 pl-3">
          <div className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
            👤 User
          </div>
          <ClampedMarkdown text={turn.user_input ?? ""} />
        </div>
      </div>

      {/* ASSISTANT — card style */}
      <div className="mt-2 flex items-start gap-3">
        <div className="w-[64px] shrink-0" />
        <div
          className={cn(
            "flex-1 rounded-md border px-3 py-2",
            hasFinalAnswer
              ? "border-border bg-muted/40"
              : "border-red-300 bg-red-50/50 dark:border-red-900/60 dark:bg-red-950/20",
          )}
        >
          <div
            className={cn(
              "text-[10px] font-semibold uppercase tracking-wide",
              hasFinalAnswer
                ? "text-muted-foreground"
                : "text-red-700 dark:text-red-300",
            )}
          >
            🎯 Assistant{!hasFinalAnswer ? " · incomplete" : ""}
          </div>
          {hasFinalAnswer ? (
            <ClampedMarkdown text={turn.final_answer ?? ""} />
          ) : (
            <div className="mt-1 text-sm italic text-muted-foreground">
              Turn ended without a final answer
            </div>
          )}
        </div>
      </div>

      <TurnMetadataStrip turn={turn} onInspect={onInspect} />
    </div>
  )
}
