import { useState } from "react"
import { Download } from "lucide-react"
import { ExtractDialog } from "./ExtractDialog"
import type { Anchor } from "./extract-defaults"

interface Props {
  anchor: Anchor
  className?: string
}

export function ExtractPacketsButton({ anchor, className }: Props) {
  const [open, setOpen] = useState(false)
  return (
    <>
      <button
        onClick={() => setOpen(true)}
        className={
          className ??
          "mr-2 flex items-center gap-1.5 rounded-md border border-border px-2 py-1 text-xs text-foreground transition-colors hover:bg-muted"
        }
        title="Extract pcap packets for this row"
      >
        <Download className="size-3.5" />
        Extract packets
      </button>
      <ExtractDialog anchor={anchor} open={open} onClose={() => setOpen(false)} />
    </>
  )
}
