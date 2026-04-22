import { formatDateTime, formatMs } from "@/lib/format"
import type { LlmCallDetail } from "@/types/api"

interface Props {
  detail: LlmCallDetail
}

export function TimelineBar({ detail }: Props) {
  const { request_time, complete_time, ttfb_ms, e2e_latency_ms } = detail

  if (!complete_time || !e2e_latency_ms) {
    return (
      <div className="rounded-lg border border-border bg-muted/30 px-4 py-3 text-sm text-muted-foreground">
        Timeline data unavailable
      </div>
    )
  }

  const ttfbRatio = (ttfb_ms ?? 0) / e2e_latency_ms
  const genRatio = 1 - ttfbRatio

  return (
    <div className="rounded-lg border border-border bg-muted/30 px-4 py-3">
      <div className="mb-2 flex justify-between text-xs text-muted-foreground">
        <span>{formatDateTime(request_time)}</span>
        <span>{formatDateTime(complete_time)}</span>
      </div>
      <div className="flex h-6 overflow-hidden rounded-md">
        {ttfbRatio > 0 && (
          <div
            className="flex items-center justify-center bg-amber-400/80 text-xs font-medium text-amber-900 dark:bg-amber-500/30 dark:text-amber-300"
            style={{ width: `${Math.max(ttfbRatio * 100, 8)}%` }}
          >
            TTFB {formatMs(ttfb_ms)}
          </div>
        )}
        {genRatio > 0 && (
          <div
            className="flex items-center justify-center bg-blue-400/80 text-xs font-medium text-blue-900 dark:bg-blue-500/30 dark:text-blue-300"
            style={{ width: `${Math.max(genRatio * 100, 8)}%` }}
          >
            Gen {formatMs(e2e_latency_ms - (ttfb_ms ?? 0))}
          </div>
        )}
      </div>
      <div className="mt-1.5 flex gap-4 text-xs text-muted-foreground">
        <span>TTFB: {formatMs(ttfb_ms)}</span>
        <span>E2E: {formatMs(e2e_latency_ms)}</span>
      </div>
    </div>
  )
}
