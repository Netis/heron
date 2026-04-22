import { useState } from "react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import type { EnrichedToolCallFull, ParsedCallContent } from "@/types/api"

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
  parsed: ParsedCallContent
}

export function CallParsedOutput({ parsed }: Props) {
  return (
    <div className="space-y-3">
      {parsed.reasoning && (
        <details className="rounded border border-border/50 p-2" open={false}>
          <summary className="cursor-pointer text-muted-foreground">Reasoning</summary>
          <pre className="mt-2 max-h-[600px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">{parsed.reasoning}</pre>
        </details>
      )}
      {parsed.message && (
        <details className="rounded border border-border/50 p-2" open>
          <summary className="cursor-pointer text-muted-foreground">Message</summary>
          <div className="mt-2 max-h-[400px] overflow-auto text-[11px]">
            <Markdown text={parsed.message} />
          </div>
        </details>
      )}
      {parsed.tool_calls && parsed.tool_calls.length > 0 && (
        <div className="rounded border border-border/50 p-2">
          <div className="mb-1 text-muted-foreground">Tool calls ({parsed.tool_calls.length})</div>
          <div className="space-y-2">
            {parsed.tool_calls.map((tc) => (
              <ToolCallRow key={tc.id} tc={tc} />
            ))}
          </div>
        </div>
      )}
    </div>
  )
}
