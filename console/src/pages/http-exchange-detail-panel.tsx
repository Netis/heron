import { X, ChevronUp, ChevronDown, Loader2 } from "lucide-react"
import { useHttpExchange } from "@/hooks/use-http-exchange"
import { formatBytes, formatDateTime, formatMs, formatNumber } from "@/lib/format"
import { StatusBadge } from "@/components/ui/status-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import { BodyViewer } from "@/components/raw-http/body-viewer"
import type { HttpExchangeDetail } from "@/types/api"
import { ExtractPacketsButton } from "@/features/pcap-extract/ExtractPacketsButton"

interface Props {
  id: string
  onClose: () => void
  onNavigate: (direction: "prev" | "next") => void
  hasPrev: boolean
  hasNext: boolean
}

function parseHeaders(raw: string | null | undefined): [string, string][] {
  if (!raw) return []
  try {
    return JSON.parse(raw)
  } catch {
    return []
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

function MetadataGrid({ detail }: { detail: HttpExchangeDetail }) {
  const duration =
    detail.response_complete_time != null
      ? (detail.response_complete_time - detail.request_time) / 1000
      : null
  const rows: [string, string][] = [
    ["ID", detail.id],
    ["Source", detail.source_id || "—"],
    ["Method", detail.method],
    ["URI", detail.uri],
    ["Client", `${detail.client_ip}:${detail.client_port}`],
    ["Server", `${detail.server_ip}:${detail.server_port}`],
    ["Request Time", formatDateTime(detail.request_time)],
    [
      "First Byte",
      detail.response_first_byte_time != null ? formatDateTime(detail.response_first_byte_time) : "—",
    ],
    [
      "Complete",
      detail.response_complete_time != null ? formatDateTime(detail.response_complete_time) : "—",
    ],
    ["Duration", duration != null ? formatMs(duration) : "—"],
    ["SSE", detail.is_sse ? "Yes" : "No"],
  ]
  if (detail.is_sse) {
    rows.push(
      ["SSE Events", formatNumber(detail.sse_event_count)],
      ["SSE Data Bytes", formatBytes(detail.sse_data_bytes)],
    )
  }

  return (
    <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 px-4 py-3 text-sm">
      {rows.map(([label, value]) => (
        <div key={label} className="contents">
          <span className="text-muted-foreground">{label}</span>
          <span className="truncate font-mono text-xs" title={value}>
            {value}
          </span>
        </div>
      ))}
    </div>
  )
}

export function HttpExchangeDetailPanel({ id, onClose, onNavigate, hasPrev, hasNext }: Props) {
  const { data: detail, isLoading, isError } = useHttpExchange(id)

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
          <h2 className="text-sm font-semibold">HTTP Log Detail</h2>
          <div className="flex items-center gap-1">
            {detail && (
              <ExtractPacketsButton anchor={{ type: "http_exchange", row: detail }} />
            )}
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
              Failed to load HTTP exchange detail
            </div>
          ) : (
            <>
              {/* Summary cards */}
              <div className="grid grid-cols-4 gap-3 p-4">
                <SummaryCard label="Method / URI">
                  <div>{detail.method}</div>
                  <div className="truncate text-xs text-muted-foreground" title={detail.uri}>
                    {detail.uri}
                  </div>
                </SummaryCard>
                <SummaryCard label="Status">
                  {detail.status != null ? (
                    <StatusBadge status={detail.status} />
                  ) : (
                    <span className="text-xs text-muted-foreground">No response</span>
                  )}
                </SummaryCard>
                <SummaryCard label="SSE">
                  {detail.is_sse ? (
                    <div className="flex flex-col">
                      <span className="text-blue-500">Yes</span>
                      <span className="text-xs text-muted-foreground tabular-nums">
                        {formatNumber(detail.sse_event_count)} events ·{" "}
                        {formatBytes(detail.sse_data_bytes)}
                      </span>
                    </div>
                  ) : (
                    <span className="text-muted-foreground">No</span>
                  )}
                </SummaryCard>
                <SummaryCard label="Duration">
                  <div className="tabular-nums">
                    {detail.response_complete_time != null
                      ? formatMs((detail.response_complete_time - detail.request_time) / 1000)
                      : "—"}
                  </div>
                </SummaryCard>
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

              <BodyViewer title="Request Body" raw={detail.request_body} />

              {detail.is_sse && detail.response_body == null ? (
                <CollapsibleSection title="Response Body" defaultOpen>
                  <p className="text-sm text-muted-foreground">
                    SSE response — raw stream not persisted.{" "}
                    {formatNumber(detail.sse_event_count)} events received,{" "}
                    {formatBytes(detail.sse_data_bytes)} of <code>data:</code> payload
                    (frame overhead excluded).
                  </p>
                </CollapsibleSection>
              ) : (
                <BodyViewer title="Response Body" raw={detail.response_body} />
              )}
            </>
          )}
        </div>
      </div>
    </>
  )
}
