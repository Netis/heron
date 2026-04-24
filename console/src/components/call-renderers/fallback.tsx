import { CollapsibleSection } from "@/components/ui/collapsible-section"

/**
 * Fallback for wire_api values the console does not recognize. Renders the
 * raw bodies as pretty-printed JSON only — no interpretation, no guesses.
 */
export interface RawJsonFallbackProps {
  wireApi: string
  requestBody: string | null
  responseBody: string | null
  hasRequestBody: boolean
}

function formatJson(raw: string | null): string {
  if (!raw) return ""
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

export function RawJsonFallback({ wireApi, requestBody, responseBody, hasRequestBody }: RawJsonFallbackProps) {
  return (
    <>
      <section className="border-l-2 border-muted-foreground/30 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
          Input · <span className="normal-case text-[10px] text-amber-700">no renderer for wire_api "{wireApi}"</span>
        </div>
        {!hasRequestBody ? (
          <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
            Request body not captured.
          </div>
        ) : (
          <div className="rounded border border-border/60 bg-background">
            <CollapsibleSection title="Request body" defaultOpen={true}>
              <pre className="max-h-[500px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
                {formatJson(requestBody)}
              </pre>
            </CollapsibleSection>
          </div>
        )}
      </section>
      <section className="border-l-2 border-emerald-500/40 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
          Output
        </div>
        <div className="rounded border border-border/60 bg-background">
          <CollapsibleSection title="Response body" defaultOpen={true}>
            <pre className="max-h-[500px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
              {formatJson(responseBody)}
            </pre>
          </CollapsibleSection>
        </div>
      </section>
    </>
  )
}
