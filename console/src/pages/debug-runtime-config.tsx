import { useState } from "react"
import { Loader2, RefreshCw, Copy, Check } from "lucide-react"
import { useRuntimeConfig } from "@/hooks/use-runtime-config"

/**
 * Developer-only page: shows the AppConfig the running process actually has
 * in memory (post env / CLI overrides), so users can tell whether the live
 * config matches the file on disk after edits-without-restart.
 */
export function RuntimeConfigPage() {
  const { data, isLoading, isFetching, refetch, error } = useRuntimeConfig()
  const [copied, setCopied] = useState(false)

  if (isLoading || !data) {
    if (error) {
      return (
        <div className="flex h-full items-center justify-center p-6 text-sm text-destructive">
          Failed to load runtime config: {String(error)}
        </div>
      )
    }
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  const json = JSON.stringify(data.config, null, 2)
  const loadedAt = new Date(data.loaded_at_ms).toLocaleString()

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(json)
      setCopied(true)
      setTimeout(() => setCopied(false), 1200)
    } catch {
      // Clipboard may be unavailable (insecure context); silently ignore.
    }
  }

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* ===== Header ===== */}
      <div className="flex flex-wrap items-center gap-3 rounded-lg border border-border bg-card p-3">
        <span className="text-sm font-semibold">Runtime Config</span>

        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span>
            version <span className="font-mono text-foreground">{data.version}</span>
          </span>
          <span>
            loaded at <span className="font-mono text-foreground">{loadedAt}</span>
          </span>
          <span className="break-all">
            from <span className="font-mono text-foreground">{data.config_path}</span>
          </span>
        </div>

        <div className="ml-auto flex items-center gap-2">
          <button
            onClick={handleCopy}
            className="flex h-7 items-center gap-1 rounded-md bg-muted px-2 text-xs text-muted-foreground hover:bg-muted/70"
          >
            {copied ? (
              <>
                <Check className="size-3" /> Copied
              </>
            ) : (
              <>
                <Copy className="size-3" /> Copy JSON
              </>
            )}
          </button>
          <button
            onClick={() => refetch()}
            className="flex h-7 items-center gap-1 rounded-md bg-muted px-2 text-xs text-muted-foreground hover:bg-muted/70"
            disabled={isFetching}
          >
            <RefreshCw className={`size-3 ${isFetching ? "animate-spin" : ""}`} />
            Refresh
          </button>
        </div>
      </div>

      {/* ===== Body: pretty JSON ===== */}
      <pre className="overflow-auto rounded-lg border border-border bg-card p-3 font-mono text-xs leading-relaxed">
        {json}
      </pre>

      <p className="px-1 text-xs text-muted-foreground">
        This is the configuration the running process has in memory — not a
        re-read of the file on disk. To pick up disk edits, restart the
        process.
      </p>
    </div>
  )
}
