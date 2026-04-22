import { useState } from "react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { InputSection } from "../input-section"
import type { CallRenderer } from "./types"
import type { JoinedToolCall } from "@/lib/wire-api-parsers"

function formatArgs(s: string): string {
  try { return JSON.stringify(JSON.parse(s), null, 2) } catch { return s }
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

function ToolCallRow({ tc }: { tc: JoinedToolCall }) {
  const [argsOpen, setArgsOpen] = useState(true)
  const [resultOpen, setResultOpen] = useState(false)
  const result = tc.result
  return (
    <div className="rounded bg-muted/40 p-2">
      <div className="font-medium">🔧 {tc.name}</div>
      <details className="mt-1" open={argsOpen} onToggle={(e) => setArgsOpen((e.target as HTMLDetailsElement).open)}>
        <summary className="cursor-pointer text-muted-foreground text-[10px]">args</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatArgs(tc.args_json)}</pre>
      </details>
      {result ? (
        <details className="mt-1" open={resultOpen} onToggle={(e) => setResultOpen((e.target as HTMLDetailsElement).open)}>
          <summary className={cn("cursor-pointer text-[10px]", result.is_error ? "text-red-600" : "text-muted-foreground")}>
            ⤷ {result.is_error ? "error" : "result"} · {formatSize(new Blob([result.content]).size)}
          </summary>
          <pre className={cn(
            "mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]",
            result.is_error && "text-red-600",
          )}>
            {result.content}
          </pre>
        </details>
      ) : (
        <div className="mt-1 text-[10px] text-muted-foreground italic">⤷ result · (no response, turn ended)</div>
      )}
    </div>
  )
}

interface OutputProps {
  reasoning: string | null
  message: string | null
  joinedToolCalls: JoinedToolCall[]
}

export function CallOutputRenderer({ reasoning, message, joinedToolCalls }: OutputProps) {
  return (
    <div className="space-y-3">
      {reasoning && (
        <details className="rounded border border-border/50 p-2" open={false}>
          <summary className="cursor-pointer text-muted-foreground">Reasoning</summary>
          <pre className="mt-2 max-h-[600px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">{reasoning}</pre>
        </details>
      )}
      {message && (
        <details className="rounded border border-border/50 p-2" open>
          <summary className="cursor-pointer text-muted-foreground">Message</summary>
          <div className="mt-2 max-h-[400px] overflow-auto text-[11px]">
            <Markdown text={message} />
          </div>
        </details>
      )}
      {joinedToolCalls.length > 0 && (
        <div className="rounded border border-border/50 p-2">
          <div className="mb-1 text-muted-foreground">Tool calls ({joinedToolCalls.length})</div>
          <div className="space-y-2">
            {joinedToolCalls.map((tc) => (
              <ToolCallRow key={tc.id} tc={tc} />
            ))}
          </div>
        </div>
      )}
    </div>
  )
}

export const DefaultCallRenderer: CallRenderer = ({ parsed, joinedToolCalls, wireApi, hasRequestBody, onOpenRawHttp }) => {
  return (
    <>
      <InputSection
        parsedInput={parsed.input}
        wireApi={wireApi}
        hasRequestBody={hasRequestBody}
        onOpenRawHttp={onOpenRawHttp}
      />
      <section className="border-l-2 border-emerald-500/40 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
          Output
        </div>
        <CallOutputRenderer
          reasoning={parsed.output.reasoning}
          message={parsed.output.message}
          joinedToolCalls={joinedToolCalls}
        />
      </section>
    </>
  )
}
