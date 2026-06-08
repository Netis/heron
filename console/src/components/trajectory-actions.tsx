import { useState } from "react"
import { Download, Loader2 } from "lucide-react"
import { cn } from "@/lib/utils"
import { downloadFile, type DownloadResult } from "@/lib/api"

/** Icon-only "download this trajectory" button (turn / session detail views). */
export function DownloadTrajectoryButton({
  url,
  fallbackName,
  title = "Download as an SFT trajectory (.jsonl)",
  className,
}: {
  url: string
  fallbackName: string
  title?: string
  className?: string
}) {
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  return (
    <button
      type="button"
      disabled={busy}
      onClick={async (e) => {
        e.stopPropagation()
        e.preventDefault()
        setBusy(true)
        setErr(null)
        try {
          await downloadFile(url, fallbackName)
        } catch (e2) {
          setErr(e2 instanceof Error ? e2.message : String(e2))
        } finally {
          setBusy(false)
        }
      }}
      title={err ?? title}
      aria-label={title}
      className={cn(
        "rounded p-1 transition-colors hover:bg-muted hover:text-foreground",
        err ? "text-destructive" : "text-muted-foreground",
        className,
      )}
    >
      {busy ? <Loader2 className="size-4 animate-spin" /> : <Download className="size-4" />}
    </button>
  )
}

/** Labeled batch-export button with inline written/skipped feedback
 * (Agent Turns list — exports every turn matching the current filters). */
export function BatchExportButton({
  url,
  fallbackName = "trajectories.jsonl",
  label = "Export trajectories",
  className,
}: {
  url: string
  fallbackName?: string
  label?: string
  className?: string
}) {
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<DownloadResult | null>(null)
  const [err, setErr] = useState<string | null>(null)

  return (
    <div className="flex items-center gap-2">
      <button
        type="button"
        disabled={busy}
        onClick={async () => {
          setBusy(true)
          setErr(null)
          setResult(null)
          try {
            setResult(await downloadFile(url, fallbackName))
          } catch (e) {
            setErr(e instanceof Error ? e.message : String(e))
          } finally {
            setBusy(false)
          }
        }}
        title="Export every turn matching the current filters as SFT trajectories (.jsonl)"
        className={cn(
          "inline-flex items-center gap-1.5 rounded-md border border-border bg-background px-2.5 py-1 text-xs font-medium text-foreground transition-colors hover:bg-muted disabled:opacity-50",
          className,
        )}
      >
        {busy ? <Loader2 className="size-3.5 animate-spin" /> : <Download className="size-3.5" />}
        {label}
      </button>
      {result && (
        <span className="text-xs text-muted-foreground">
          {result.written}/{result.total}
          {result.skipped > 0 && <span className="text-amber-500"> · {result.skipped} skipped</span>}
        </span>
      )}
      {err && <span className="text-xs text-destructive">{err}</span>}
    </div>
  )
}
