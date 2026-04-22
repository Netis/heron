import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import type { ParsedContentBlock, ParsedMessage, ParsedRole } from "@/types/api"

const PREVIEW_CHARS = 120

const ROLE_STYLES: Record<ParsedRole, string> = {
  system: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  user: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  assistant: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
  tool: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
}

function formatJson(raw: string): string {
  try { return JSON.stringify(JSON.parse(raw), null, 2) } catch { return raw }
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

function previewOfContent(content: ParsedContentBlock[]): string {
  for (const b of content) {
    switch (b.type) {
      case "text":
        return b.text.slice(0, PREVIEW_CHARS)
      case "tool_use":
        return `🔧 ${b.name}(${b.args_json.slice(0, 60)}${b.args_json.length > 60 ? "…" : ""})`
      case "tool_result":
        return `⤷ ${b.tool_use_id} · ${formatSize(b.content.length)}`
      case "image":
        return `🖼️ image${b.mime ? ` (${b.mime})` : ""}`
    }
  }
  return ""
}

function ContentBlockView({ block }: { block: ParsedContentBlock }) {
  switch (block.type) {
    case "text":
      return <div className="text-[11px]"><Markdown text={block.text} /></div>
    case "tool_use":
      return (
        <div className="rounded bg-muted/40 p-2 text-[11px]">
          <div className="font-medium">🔧 {block.name}</div>
          <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(block.args_json)}</pre>
        </div>
      )
    case "tool_result":
      return (
        <div className="rounded bg-muted/40 p-2 text-[11px]">
          <div className={cn("mb-1", block.is_error && "text-red-600")}>
            ⤷ {block.is_error ? "error" : "result"} · {formatSize(block.content.length)} · <span className="text-muted-foreground">{block.tool_use_id}</span>
          </div>
          <pre className={cn(
            "max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]",
            block.is_error && "text-red-600",
          )}>{block.content}</pre>
        </div>
      )
    case "image":
      return (
        <div className="text-[11px] text-muted-foreground italic">
          🖼️ image{block.mime ? ` (${block.mime})` : ""}{block.size_bytes != null ? ` · ${formatSize(block.size_bytes)}` : ""}
        </div>
      )
    default: {
      // Runtime fallback — backend can send ParsedContentBlock::Unknown variants
      // whose `type` isn't in the TS union. TS sees `block` as `never` here, so
      // we cast once to access the raw payload.
      const unknown = block as unknown as { type: string }
      return (
        <details className="text-[11px]">
          <summary className="cursor-pointer text-red-600">⚠️ unknown block: {String(unknown.type)}</summary>
          <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{JSON.stringify(block, null, 2)}</pre>
        </details>
      )
    }
  }
}

function MessageRow({ msg, index }: { msg: ParsedMessage; index: number }) {
  const [open, setOpen] = useState(false)
  const preview = previewOfContent(msg.content)
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        <span className="w-5 shrink-0 text-[10px] tabular-nums text-muted-foreground">#{index + 1}</span>
        <span className={cn("shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium", ROLE_STYLES[msg.role])}>
          {msg.role}
        </span>
        <span className="flex-1 truncate text-muted-foreground">{preview}</span>
        {open ? <ChevronDown className="size-3 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3 shrink-0 text-muted-foreground" />}
      </button>
      {open && (
        <div className="space-y-2 border-t border-border/30 bg-muted/10 px-3 py-2">
          {msg.content.map((b, i) => <ContentBlockView key={i} block={b} />)}
        </div>
      )}
    </div>
  )
}

interface Props {
  messages: ParsedMessage[]
}

export function MessagesBlock({ messages }: Props) {
  const [open, setOpen] = useState(true)
  if (messages.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Messages</span>
        <span className="text-muted-foreground">({messages.length})</span>
      </button>
      {open && (
        <div>
          {messages.map((m, i) => <MessageRow key={i} msg={m} index={i} />)}
        </div>
      )}
    </div>
  )
}
