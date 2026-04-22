import { useMemo, useState } from "react"
import { ChevronRight, ChevronDown, Wrench, MessageSquare, Target } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { CallOutputRenderer } from "@/components/llm-call-detail/renderers"
import {
  deriveCallPreview,
  getParser,
  joinToolResults,
  parseCall,
  parseJsonOrNull,
  type CallPreview,
  type ParsedToolResult,
} from "@/lib/wire-api-parsers"
import type { AgentTurnCallItem } from "@/types/api"

const SLOW_THRESHOLD_MS = 10_000

function TypeChip({ preview, toolNames }: { preview: CallPreview; toolNames: string[] }) {
  if (preview.type === "tool_call") {
    const more = preview.toolCalls.length - toolNames.length
    return (
      <span className="flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-300">
        <Wrench className="size-3" />
        {toolNames.join(", ")}
        {more > 0 && <span className="ml-1 opacity-70">+{more}</span>}
      </span>
    )
  }
  if (preview.type === "final") {
    return (
      <span className="flex items-center gap-1 rounded bg-emerald-100 px-1.5 py-0.5 text-[10px] font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300">
        <Target className="size-3" /> final
      </span>
    )
  }
  return (
    <span className="flex items-center gap-1 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
      <MessageSquare className="size-3" /> text
    </span>
  )
}

function classify(call: AgentTurnCallItem): "normal" | "slow" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}

interface Props {
  call: AgentTurnCallItem
  /** Next call in the same turn, for tool-result join. */
  nextCall: AgentTurnCallItem | null
  finalCallId: string | null
  active?: boolean
  defaultExpanded?: boolean
  onOpenRawHttp?: (id: string) => void
}

export function CallCard({ call, nextCall, finalCallId, active, defaultExpanded, onOpenRawHttp }: Props) {
  const [expanded, setExpanded] = useState(Boolean(defaultExpanded))
  const speed = classify(call)

  const preview = useMemo(
    () => deriveCallPreview(call.wire_api, call.response_body, call.id, finalCallId),
    [call.wire_api, call.response_body, call.id, finalCallId],
  )
  const toolNames = preview.toolCalls.slice(0, 2).map((t) => t.name)
  const firstToolArgs = preview.toolCalls[0]?.args_json

  const parsedOutput = useMemo(() => {
    if (!expanded) return null
    const pc = parseCall(call.wire_api, call.request_body, call.response_body)
    const parser = getParser(call.wire_api)
    const nextResults: ParsedToolResult[] = parser && nextCall
      ? parser.parseInput(parseJsonOrNull(nextCall.request_body)).tool_results
      : []
    const joined = joinToolResults(pc.output.tool_calls, nextResults)
    return { reasoning: pc.output.reasoning, message: pc.output.message, joinedToolCalls: joined }
  }, [expanded, call.wire_api, call.request_body, call.response_body, nextCall])

  const previewText = preview.messagePreview ?? (firstToolArgs ? truncate(firstToolArgs, 200) : null)

  return (
    <div
      id={`call-${call.sequence}`}
      className={cn(
        "rounded-lg border bg-background transition-colors",
        speed === "slow" && "border-l-2 border-l-amber-500/70 border-border",
        speed === "error" && "border-l-2 border-l-red-500/70 border-border",
        speed === "normal" && "border-border",
        active && "ring-2 ring-blue-400 ring-offset-1",
      )}
    >
      <button
        onClick={() => setExpanded((e) => !e)}
        className="w-full text-left"
      >
        <div className="flex w-full flex-col gap-1 px-3 py-2 text-left">
          <div className="flex items-center gap-3">
            <span className="w-6 shrink-0 tabular-nums text-xs text-muted-foreground">#{call.sequence}</span>
            <TypeChip preview={preview} toolNames={toolNames} />
            <span className="flex-1 truncate text-xs text-muted-foreground">{call.model}</span>
            <span className={cn(
              "shrink-0 text-xs tabular-nums",
              speed === "slow" && "text-amber-600",
              speed === "error" && "text-red-600",
              speed === "normal" && "text-muted-foreground",
            )}>
              {speed === "error" && "✗ "}{formatMs(call.e2e_latency_ms)}
            </span>
            <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
              {formatNumber(call.input_tokens)}↑ {formatNumber(call.output_tokens)}↓
            </span>
            {expanded ? <ChevronDown className="size-4 text-muted-foreground" /> : <ChevronRight className="size-4 text-muted-foreground" />}
          </div>
          {previewText && (
            <div className="truncate pl-9 text-[11px] text-muted-foreground">
              {preview.messagePreview
                ? `"${preview.messagePreview}${preview.messagePreview.length >= 60 ? "…" : ""}"`
                : previewText}
            </div>
          )}
        </div>
      </button>
      {expanded && parsedOutput && (
        <div className="border-t border-border px-3 py-2 space-y-3 text-xs">
          <CallOutputRenderer
            reasoning={parsedOutput.reasoning}
            message={parsedOutput.message}
            joinedToolCalls={parsedOutput.joinedToolCalls}
          />
          <div className="text-muted-foreground">
            {call.wire_api} · TTFB {formatMs(call.ttfb_ms)} · finish: {call.finish_reason ?? "—"}
          </div>
          <button onClick={() => onOpenRawHttp?.(call.id)} className="text-foreground hover:underline">View raw HTTP →</button>
        </div>
      )}
    </div>
  )
}

function truncate(s: string, n: number): string {
  return s.length <= n ? s : s.slice(0, n)
}
