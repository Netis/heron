import { Download } from "lucide-react"
import {
  buildAgentTurnPacketsUrl,
  buildExtractUrl,
  defaultsFor,
  validate,
  type Anchor,
} from "./extract-defaults"

interface Props {
  anchor: Anchor
  className?: string
}

export function ExtractPacketsButton({ anchor, className }: Props) {
  const onDownload = () => {
    const href = anchor.type === "agent_turn"
      ? buildAgentTurnPacketsUrl(anchor.row)
      : buildValidatedExtractUrl(anchor)
    if (!href) return

    const a = document.createElement("a")
    a.href = href
    a.download = ""    // honor Content-Disposition filename
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
  }

  const title = anchor.type === "agent_turn"
    ? "Download pcap packets matching this agent turn"
    : "Download pcap packets matching this row"

  return (
    <button
      onClick={onDownload}
      className={
        className ??
        "mr-2 flex items-center gap-1.5 rounded-md border border-border px-2 py-1 text-xs text-foreground transition-colors hover:bg-muted"
      }
      title={title}
    >
      <Download className="size-3.5" />
      Download Packets
    </button>
  )
}

function buildValidatedExtractUrl(anchor: Exclude<Anchor, { type: "agent_turn" }>): string | null {
  const values = defaultsFor(anchor)
  const result = validate(values)
  if (!result.ok) {
    window.alert(`Cannot download packets: ${result.reason}`)
    return null
  }
  return buildExtractUrl(values)
}
