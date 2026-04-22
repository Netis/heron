import { useState } from "react"
import { ChevronRight, ChevronDown, Wrench, MessageSquare, Target } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { useLlmCallDetail } from "@/hooks/use-llm-call-detail"
import { Markdown } from "@/components/ui/markdown"
import type { AgentTurnCallItem, EnrichedToolCallFull } from "@/types/api"

const SLOW_THRESHOLD_MS = 10_000

function TypeChip({ call }: { call: AgentTurnCallItem }) {
  if (call.type === "tool_call") {
    const names = call.tool_calls.slice(0, 2).map(t => t.name)
    const more = call.tool_calls.length - names.length
    return (
      <span className="flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-300">
        <Wrench className="size-3" />
        {names.join(", ")}
        {more > 0 && <span className="ml-1 opacity-70">+{more}</span>}
      </span>
    )
  }
  if (call.type === "final") {
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

function formatArgs(s: string): string {
  try { return JSON.stringify(JSON.parse(s), null, 2) } catch { return s }
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

function ToolCallRow({ tc }: { tc: EnrichedToolCallFull }) {
  const [argsOpen, setArgsOpen] = useState(true)
  const [resultOpen, setResultOpen] = useState(false)
  return (
    <div className="rounded bg-muted/40 p-2">
      <div className="font-medium">🔧 {tc.name}</div>
      <details className="mt-1" open={argsOpen} onToggle={(e) => setArgsOpen((e.target as HTMLDetailsElement).open)}>
        <summary className="cursor-pointer text-muted-foreground text-[10px]">args</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatArgs(tc.args_json)}</pre>
      </details>
      {tc.result ? (
        <details className="mt-1" open={resultOpen} onToggle={(e) => setResultOpen((e.target as HTMLDetailsElement).open)}>
          <summary className={cn("cursor-pointer text-[10px]", tc.result.is_error ? "text-red-600" : "text-muted-foreground")}>
            ⤷ {tc.result.is_error ? "error" : "result"} · {formatSize(tc.result.size_bytes)}
          </summary>
          <pre className={cn(
            "mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]",
            tc.result.is_error && "text-red-600",
          )}>
            {tc.result.content}
          </pre>
        </details>
      ) : (
        <div className="mt-1 text-[10px] text-muted-foreground italic">⤷ result · (no response, turn ended)</div>
      )}
    </div>
  )
}

interface Props {
  call: AgentTurnCallItem
  active?: boolean
  defaultExpanded?: boolean
  onOpenRawHttp?: (id: string) => void
}

export function CallCard({ call, active, defaultExpanded, onOpenRawHttp }: Props) {
  const [expanded, setExpanded] = useState(Boolean(defaultExpanded))
  const { data: detail } = useLlmCallDetail(expanded ? call.id : null)
  const speed = classify(call)
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
            <TypeChip call={call} />
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
          {(call.message_preview ?? call.tool_calls[0]?.args_preview) && (
            <div className="truncate pl-9 text-[11px] text-muted-foreground">
              {call.message_preview
                ? `"${call.message_preview}${(call.message_preview?.length ?? 0) >= 60 ? "…" : ""}"`
                : call.tool_calls[0].args_preview}
            </div>
          )}
        </div>
      </button>
      {expanded && (
        <div className="border-t border-border px-3 py-2 space-y-3 text-xs">
          {detail?.parsed.reasoning && (
            <details className="rounded border border-border/50 p-2" open={false}>
              <summary className="cursor-pointer text-muted-foreground">Reasoning</summary>
              <pre className="mt-2 max-h-[600px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">{detail.parsed.reasoning}</pre>
            </details>
          )}
          {detail?.parsed.message && (
            <details className="rounded border border-border/50 p-2" open>
              <summary className="cursor-pointer text-muted-foreground">Message</summary>
              <div className="mt-2 max-h-[400px] overflow-auto text-[11px]">
                <Markdown text={detail.parsed.message} />
              </div>
            </details>
          )}
          {detail?.parsed.tool_calls && detail.parsed.tool_calls.length > 0 && (
            <div className="rounded border border-border/50 p-2">
              <div className="mb-1 text-muted-foreground">Tool calls ({detail.parsed.tool_calls.length})</div>
              <div className="space-y-2">
                {detail.parsed.tool_calls.map((tc) => (
                  <ToolCallRow key={tc.id} tc={tc} />
                ))}
              </div>
            </div>
          )}
          <div className="text-muted-foreground">
            {call.wire_api} · TTFB {formatMs(call.ttfb_ms)} · finish: {call.finish_reason ?? "—"}
          </div>
          <button onClick={() => onOpenRawHttp?.(call.id)} className="text-foreground hover:underline">View raw HTTP →</button>
        </div>
      )}
    </div>
  )
}
