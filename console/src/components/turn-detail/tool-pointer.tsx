import { AlertTriangle, ChevronRight, ChevronDown } from "lucide-react"
import { useState } from "react"
import { cn } from "@/lib/utils"
import type {
  ToolUseState,
  ToolResultState,
  ToolOrigin,
  ToolResolution,
} from "@/lib/turn-index"

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

// ── ToolUsePointer — echoes the result inline under a tool_use block. ──────

interface ToolUsePointerProps {
  state: ToolUseState
  /** Full resolution; used to render the inline echo body. Null when state != "healthy". */
  resolution: ToolResolution | null
  className?: string
}

export function ToolUsePointer({ state, resolution, className }: ToolUsePointerProps) {
  const [open, setOpen] = useState(false)

  if (state === "healthy" && resolution != null) {
    return (
      <div className={cn("text-[11px]", className)}>
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          className="flex w-full items-center gap-1 text-left text-blue-700 hover:underline dark:text-blue-400"
        >
          {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
          <span>→ result in #{resolution.call_sequence} ✓ · {formatSize(resolution.size_bytes)}</span>
          {resolution.is_error && <span className="text-red-600 dark:text-red-400">· error</span>}
        </button>
        {open && (
          <pre
            className={cn(
              "mt-1 max-h-[320px] overflow-auto whitespace-pre-wrap rounded border border-border/60 bg-muted/30 px-2 py-1 font-mono text-[10px]",
              resolution.is_error && "border-red-300 bg-red-50 text-red-700 dark:border-red-900/40 dark:bg-red-900/10 dark:text-red-300",
            )}
          >
            {resolution.content}
          </pre>
        )}
      </div>
    )
  }
  if (state === "legit_pending") {
    return <span className={cn("text-[11px] text-muted-foreground", className)}>→ no response (turn ended)</span>
  }
  return (
    <span className={cn("inline-flex items-center gap-1 text-[11px] font-medium text-amber-700 dark:text-amber-400", className)}>
      <AlertTriangle className="size-3" />
      → result not captured
    </span>
  )
}

// ── ToolResultBackLink — echoes the originating tool_use args below a tool_result. ──

interface ToolResultBackLinkProps {
  state: ToolResultState
  /** Full origin; used to render the inline args echo. Null when state != "healthy". */
  origin: ToolOrigin | null
  className?: string
}

export function ToolResultBackLink({ state, origin, className }: ToolResultBackLinkProps) {
  const [open, setOpen] = useState(false)

  if (state === "healthy" && origin != null) {
    return (
      <div className={cn("text-[11px]", className)}>
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          className="flex w-full items-center gap-1 text-left text-blue-700 hover:underline dark:text-blue-400"
        >
          {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
          <span>← from #{origin.call_sequence} · {origin.tool_name}</span>
        </button>
        {open && (
          <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap rounded border border-border/60 bg-muted/30 px-2 py-1 font-mono text-[10px]">
            {origin.args_json}
          </pre>
        )}
      </div>
    )
  }
  return (
    <span className={cn("inline-flex items-center gap-1 text-[11px] font-medium text-amber-700 dark:text-amber-400", className)}>
      <AlertTriangle className="size-3" />
      ← origin not captured
    </span>
  )
}
