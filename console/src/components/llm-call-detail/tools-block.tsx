import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import type { ParsedToolDef } from "@/types/api"

interface Props {
  tools: ParsedToolDef[]
}

function formatJson(raw: string): string {
  try { return JSON.stringify(JSON.parse(raw), null, 2) } catch { return raw }
}

function ToolRow({ tool }: { tool: ParsedToolDef }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">{tool.name}</span>
        {tool.description && (
          <span className="truncate text-muted-foreground" title={tool.description}>
            — {tool.description}
          </span>
        )}
      </button>
      {open && (
        <pre className="mx-3 mb-2 max-h-[300px] overflow-auto rounded bg-muted p-2 font-mono text-[10px]">
          {formatJson(tool.input_schema_json || "{}")}
        </pre>
      )}
    </div>
  )
}

export function ToolsBlock({ tools }: Props) {
  const [open, setOpen] = useState(false)
  if (tools.length === 0) return null
  const teaser = tools.slice(0, 3).map((t) => t.name).join(", ")
  const more = tools.length > 3 ? `, +${tools.length - 3}` : ""
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Tools</span>
        <span className="text-muted-foreground">({tools.length})</span>
        {!open && (
          <span className="truncate text-muted-foreground">— {teaser}{more}</span>
        )}
      </button>
      {open && (
        <div className="border-t border-border/40">
          {tools.map((t) => <ToolRow key={t.name} tool={t} />)}
        </div>
      )}
    </div>
  )
}
