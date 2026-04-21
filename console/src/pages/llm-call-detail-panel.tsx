import { X, ChevronUp, ChevronDown, Loader2 } from "lucide-react"
import { useLlmCallDetail } from "@/hooks/use-llm-call-detail"
import { formatDateTime, formatMs, formatNumber } from "@/lib/format"
import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import type { LlmCallDetail } from "@/types/api"

interface Props {
  id: string
  onClose: () => void
  onNavigate: (direction: "prev" | "next") => void
  hasPrev: boolean
  hasNext: boolean
}

function parseHeaders(raw: string | null): [string, string][] {
  if (!raw) return []
  try {
    return JSON.parse(raw)
  } catch {
    return []
  }
}

function formatJson(raw: string | null): string {
  if (!raw) return ""
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

function SummaryCard({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1 rounded-lg border border-border bg-muted/30 px-3 py-2">
      <span className="text-xs text-muted-foreground">{label}</span>
      <div className="text-sm font-medium">{children}</div>
    </div>
  )
}

function TimelineBar({ detail }: { detail: LlmCallDetail }) {
  const { request_time, response_time, complete_time, ttfb_ms, e2e_latency_ms } = detail

  if (!response_time || !complete_time || !e2e_latency_ms) {
    return (
      <div className="rounded-lg border border-border bg-muted/30 px-4 py-3 text-sm text-muted-foreground">
        Timeline data unavailable
      </div>
    )
  }

  const ttfbRatio = (ttfb_ms ?? 0) / e2e_latency_ms
  const genRatio = 1 - ttfbRatio

  return (
    <div className="rounded-lg border border-border bg-muted/30 px-4 py-3">
      <div className="mb-2 flex justify-between text-xs text-muted-foreground">
        <span>{formatDateTime(request_time)}</span>
        <span>{formatDateTime(complete_time)}</span>
      </div>
      <div className="flex h-6 overflow-hidden rounded-md">
        {ttfbRatio > 0 && (
          <div
            className="flex items-center justify-center bg-amber-400/80 text-xs font-medium text-amber-900 dark:bg-amber-500/30 dark:text-amber-300"
            style={{ width: `${Math.max(ttfbRatio * 100, 8)}%` }}
          >
            TTFB {formatMs(ttfb_ms)}
          </div>
        )}
        {genRatio > 0 && (
          <div
            className="flex items-center justify-center bg-blue-400/80 text-xs font-medium text-blue-900 dark:bg-blue-500/30 dark:text-blue-300"
            style={{ width: `${Math.max(genRatio * 100, 8)}%` }}
          >
            Gen {formatMs(e2e_latency_ms! - (ttfb_ms ?? 0))}
          </div>
        )}
      </div>
      <div className="mt-1.5 flex gap-4 text-xs text-muted-foreground">
        <span>TTFB: {formatMs(ttfb_ms)}</span>
        <span>E2E: {formatMs(e2e_latency_ms)}</span>
      </div>
    </div>
  )
}

function HeadersTable({ headers }: { headers: [string, string][] }) {
  return (
    <table className="w-full text-sm">
      <tbody>
        {headers.map(([key, value], i) => (
          <tr key={i} className="border-b border-border/30">
            <td className="w-[200px] py-1 pr-3 font-mono text-xs text-muted-foreground">{key}</td>
            <td className="break-all py-1 font-mono text-xs">{value}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function MetadataGrid({ detail }: { detail: LlmCallDetail }) {
  const rows = [
    ["ID", detail.id],
    ["Response ID", detail.response_id ?? "—"],
    ["Path", detail.request_path],
    ["Client", `${detail.client_ip}:${detail.client_port}`],
    ["Server", `${detail.server_ip}:${detail.server_port}`],
    ["Stream", detail.is_stream ? "Yes" : "No"],
    ["API Type", detail.api_type],
    ["Tenant", detail.tenant_id ?? "—"],
  ]

  return (
    <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 px-4 py-3 text-sm">
      {rows.map(([label, value]) => (
        <div key={label} className="contents">
          <span className="text-muted-foreground">{label}</span>
          <span className="truncate font-mono text-xs" title={String(value)}>
            {value}
          </span>
        </div>
      ))}
    </div>
  )
}

export function LlmCallDetailPanel({ id, onClose, onNavigate, hasPrev, hasNext }: Props) {
  const { data: detail, isLoading, isError } = useLlmCallDetail(id)

  const requestHeaders = detail ? parseHeaders(detail.request_headers) : []
  const responseHeaders = detail ? parseHeaders(detail.response_headers) : []

  return (
    <>
      {/* Backdrop */}
      <div className="fixed inset-0 z-40 bg-black/20" onClick={onClose} />

      {/* Panel */}
      <div className="fixed top-0 right-0 z-50 flex h-full w-[60%] min-w-[480px] flex-col border-l border-border bg-background shadow-xl animate-in slide-in-from-right duration-200">
        {/* Header */}
        <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">
          <h2 className="text-sm font-semibold">LLM Call Detail</h2>
          <div className="flex items-center gap-1">
            <button
              onClick={() => onNavigate("prev")}
              disabled={!hasPrev}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronUp className="size-4" />
            </button>
            <button
              onClick={() => onNavigate("next")}
              disabled={!hasNext}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronDown className="size-4" />
            </button>
            <button
              onClick={onClose}
              className="ml-2 rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <X className="size-4" />
            </button>
          </div>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto">
          {isLoading ? (
            <div className="flex h-40 items-center justify-center">
              <Loader2 className="size-5 animate-spin text-muted-foreground" />
            </div>
          ) : isError || !detail ? (
            <div className="flex h-40 items-center justify-center text-destructive">
              Failed to load LLM call detail
            </div>
          ) : (
            <>
              {/* Summary cards */}
              <div className="grid grid-cols-4 gap-3 p-4">
                <SummaryCard label="Wire API / Model">
                  <div>{detail.wire_api}</div>
                  <div className="truncate text-xs text-muted-foreground" title={detail.model}>
                    {detail.model}
                  </div>
                </SummaryCard>
                <SummaryCard label="Status / Finish">
                  <div className="flex items-center gap-2">
                    <StatusBadge status={detail.status_code} />
                    <FinishBadge reason={detail.finish_reason} />
                  </div>
                </SummaryCard>
                <SummaryCard label="TTFB / E2E">
                  <div className="tabular-nums">{formatMs(detail.ttfb_ms)}</div>
                  <div className="text-xs tabular-nums text-muted-foreground">
                    {formatMs(detail.e2e_latency_ms)}
                  </div>
                </SummaryCard>
                <SummaryCard label="Tokens">
                  <div className="flex items-center gap-3 tabular-nums">
                    <span className="flex flex-col">
                      <span className="text-[10px] text-muted-foreground">in</span>
                      <span>{formatNumber(detail.input_tokens)}</span>
                    </span>
                    <span className="flex flex-col">
                      <span className="text-[10px] text-muted-foreground">out</span>
                      <span>{formatNumber(detail.output_tokens)}</span>
                    </span>
                  </div>
                  <div className="text-xs tabular-nums text-muted-foreground">
                    total: {formatNumber(detail.total_tokens)}
                  </div>
                </SummaryCard>
              </div>

              {/* Timeline */}
              <div className="px-4 pb-4">
                <TimelineBar detail={detail} />
              </div>

              {/* Metadata */}
              <MetadataGrid detail={detail} />

              {/* Collapsible sections */}
              <CollapsibleSection title="Request Headers" count={requestHeaders.length}>
                {requestHeaders.length > 0 ? (
                  <HeadersTable headers={requestHeaders} />
                ) : (
                  <p className="text-sm text-muted-foreground">No headers</p>
                )}
              </CollapsibleSection>

              <CollapsibleSection title="Response Headers" count={responseHeaders.length}>
                {responseHeaders.length > 0 ? (
                  <HeadersTable headers={responseHeaders} />
                ) : (
                  <p className="text-sm text-muted-foreground">No headers</p>
                )}
              </CollapsibleSection>

              <CollapsibleSection title="Request Body">
                {detail.request_body ? (
                  <pre className="max-h-[400px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
                    {formatJson(detail.request_body)}
                  </pre>
                ) : (
                  <p className="text-sm text-muted-foreground">No body</p>
                )}
              </CollapsibleSection>

              <CollapsibleSection title="Response Body">
                {detail.response_body ? (
                  <pre className="max-h-[400px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
                    {formatJson(detail.response_body)}
                  </pre>
                ) : (
                  <p className="text-sm text-muted-foreground">No body</p>
                )}
              </CollapsibleSection>
            </>
          )}
        </div>
      </div>
    </>
  )
}
