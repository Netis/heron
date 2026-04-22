import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { Markdown } from "@/components/ui/markdown"

interface Props {
  system: string | null
}

export function SystemBlock({ system }: Props) {
  const [open, setOpen] = useState(false)
  if (!system) return null
  const chars = system.length
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">System Prompt</span>
        <span className="text-muted-foreground">({chars.toLocaleString()} chars)</span>
      </button>
      {open && (
        <div className="max-h-[400px] overflow-auto border-t border-border/40 p-3">
          <Markdown text={system} />
        </div>
      )}
    </div>
  )
}
