import { useState, useMemo } from "react"
import { X, Loader2, ChevronLeft } from "lucide-react"
import { cn } from "@/lib/utils"
import { useTurnDetail, useTurnCalls } from "@/hooks/use-turns"
import { useRequestDetail } from "@/hooks/use-request-detail"
import { formatDateTimeMs, formatMs, formatNumber, formatDuration } from "@/lib/format"
import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import { Markdown } from "@/components/ui/markdown"
import type { TurnDetail, TurnCallItem, CallDetail } from "@/types/api"

type TurnTab = "input" | "answer" | "timeline"

interface Props {
  id: string
  onClose: () => void
}

function SummaryCard({
  label,
  children,
  className,
}: {
  label: string
  children: React.ReactNode
  className?: string
}) {
  return (
    <div
      className={cn(
        "flex flex-col gap-1 rounded-lg border border-border bg-muted/30 px-3 py-2",
        className,
      )}
    >
      <span className="text-xs text-muted-foreground">{label}</span>
      <div className="text-sm font-medium">{children}</div>
    </div>
  )
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

/** Mini timeline bar showing TTFB (amber) + generation (blue) inside a call card */
function MiniTimelineBar({ call }: { call: TurnCallItem }) {
  const { ttfb_ms, e2e_latency_ms } = call
  if (!e2e_latency_ms || e2e_latency_ms <= 0) {
    return <div className="h-1.5 rounded bg-muted" />
  }
  const ttfb = ttfb_ms ?? 0
  const ttfbRatio = Math.min(ttfb / e2e_latency_ms, 1)

  return (
    <div className="flex h-1.5 overflow-hidden rounded bg-muted">
      <div
        className="bg-amber-400 dark:bg-amber-500/60"
        style={{ width: `${Math.max(ttfbRatio * 100, 2)}%` }}
      />
      <div
        className="bg-blue-400 dark:bg-blue-500/60"
        style={{ width: `${Math.max((1 - ttfbRatio) * 100, 2)}%` }}
      />
    </div>
  )
}

function CallCard({
  call,
  selected,
  onClick,
}: {
  call: TurnCallItem
  selected: boolean
  onClick: () => void
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex w-full flex-col gap-1.5 rounded-lg border px-3 py-2 text-left transition-colors",
        selected
          ? "border-foreground/20 bg-muted"
          : "border-border hover:border-foreground/10 hover:bg-muted/50",
      )}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="flex items-center gap-2 text-xs font-medium">
          <span className="flex size-5 shrink-0 items-center justify-center rounded-full bg-muted text-[10px] tabular-nums text-muted-foreground">
            {call.sequence}
          </span>
          <FinishBadge reason={call.finish_reason} />
        </span>
        <span className="shrink-0 tabular-nums text-[11px] text-muted-foreground">
          {formatDateTimeMs(call.request_time)}
        </span>
      </div>

      <div className="truncate text-xs text-muted-foreground" title={call.model}>
        {call.model}
      </div>

      <MiniTimelineBar call={call} />

      <div className="flex items-center justify-between text-[11px] tabular-nums text-muted-foreground">
        <span>
          TTFB {formatMs(call.ttfb_ms)} · E2E {formatMs(call.e2e_latency_ms)}
        </span>
        <span>
          {formatNumber(call.input_tokens)} / {formatNumber(call.output_tokens)}
        </span>
      </div>
    </button>
  )
}

/** Gantt-style timeline showing each call as a bar on a shared time axis */
function TurnGantt({ calls }: { calls: TurnCallItem[] }) {
  const { minStart, maxEnd, total } = useMemo(() => {
    if (calls.length === 0) return { minStart: 0, maxEnd: 0, total: 0 }
    let minStart = Infinity
    let maxEnd = -Infinity
    for (const c of calls) {
      minStart = Math.min(minStart, c.request_time)
      const end = c.complete_time ?? c.response_time ?? c.request_time
      maxEnd = Math.max(maxEnd, end)
    }
    const total = Math.max(maxEnd - minStart, 1)
    return { minStart, maxEnd, total }
  }, [calls])

  if (calls.length === 0) {
    return (
      <div className="rounded-lg border border-border bg-muted/30 px-4 py-6 text-center text-sm text-muted-foreground">
        No calls in this turn
      </div>
    )
  }

  return (
    <div className="rounded-lg border border-border bg-muted/30 p-4">
      <div className="mb-2 flex justify-between text-xs text-muted-foreground tabular-nums">
        <span>{formatDateTimeMs(minStart)}</span>
        <span>{formatDateTimeMs(maxEnd)}</span>
      </div>
      <div className="flex flex-col gap-1.5">
        {calls.map((call) => {
          const callEnd = call.complete_time ?? call.response_time ?? call.request_time
          const offsetRatio = (call.request_time - minStart) / total
          const widthRatio = Math.max((callEnd - call.request_time) / total, 0.004)

          const callDurMs = callEnd - call.request_time
          const ttfb = call.ttfb_ms ?? 0
          const ttfbRatio = callDurMs > 0 ? Math.min(ttfb / callDurMs, 1) : 0

          return (
            <div key={call.id} className="flex items-center gap-2">
              <span className="w-6 shrink-0 text-right text-[11px] tabular-nums text-muted-foreground">
                #{call.sequence}
              </span>
              <div className="relative flex-1">
                <div className="h-5 rounded bg-muted/60" />
                <div
                  className="absolute top-0 flex h-5 overflow-hidden rounded"
                  style={{
                    left: `${offsetRatio * 100}%`,
                    width: `${widthRatio * 100}%`,
                    minWidth: "3px",
                  }}
                >
                  <div
                    className="bg-amber-400 dark:bg-amber-500/60"
                    style={{ width: `${Math.max(ttfbRatio * 100, 3)}%` }}
                  />
                  <div
                    className="bg-blue-400 dark:bg-blue-500/60"
                    style={{ width: `${Math.max((1 - ttfbRatio) * 100, 3)}%` }}
                  />
                </div>
              </div>
              <span className="w-16 shrink-0 text-right text-[11px] tabular-nums text-muted-foreground">
                {formatMs(call.e2e_latency_ms)}
              </span>
            </div>
          )
        })}
      </div>
      <div className="mt-3 flex items-center gap-3 text-[11px] text-muted-foreground">
        <span className="flex items-center gap-1.5">
          <span className="size-2 rounded bg-amber-400 dark:bg-amber-500/60" /> TTFB
        </span>
        <span className="flex items-center gap-1.5">
          <span className="size-2 rounded bg-blue-400 dark:bg-blue-500/60" /> Generation
        </span>
      </div>
    </div>
  )
}

function TabButton({
  active,
  onClick,
  label,
  badge,
}: {
  active: boolean
  onClick: () => void
  label: string
  badge?: string | number
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "relative -mb-px border-b-2 px-3 py-2 text-sm font-medium transition-colors",
        active
          ? "border-foreground text-foreground"
          : "border-transparent text-muted-foreground hover:text-foreground",
      )}
    >
      <span className="inline-flex items-center gap-1.5">
        {label}
        {badge != null && (
          <span
            className={cn(
              "rounded-full px-1.5 py-0.5 text-[10px] tabular-nums",
              active ? "bg-foreground text-background" : "bg-muted text-muted-foreground",
            )}
          >
            {badge}
          </span>
        )}
      </span>
    </button>
  )
}

function EmptyTabContent({ label }: { label: string }) {
  return (
    <div className="flex h-full items-center justify-center py-10 text-sm text-muted-foreground">
      No {label}
    </div>
  )
}

function TurnDetailView({ turn, calls }: { turn: TurnDetail; calls: TurnCallItem[] }) {
  // Initial tab: prefer answer if present, else input, else timeline
  const initialTab: TurnTab = turn.final_answer ? "answer" : turn.user_input ? "input" : "timeline"
  const [tab, setTab] = useState<TurnTab>(initialTab)

  const metadataRows: [string, string][] = [
    ["Turn ID", turn.turn_id],
    ["Session ID", turn.session_id],
    ["Client", turn.client_kind],
    ["Tenant", turn.tenant_id ?? "—"],
    ["Start", formatDateTimeMs(turn.start_time)],
    ["End", formatDateTimeMs(turn.end_time)],
  ]
  if (turn.models_used.length > 0) {
    metadataRows.push(["Models", turn.models_used.join(", ")])
  }
  if (turn.subagents_used.length > 0) {
    metadataRows.push(["Subagents", turn.subagents_used.join(", ")])
  }

  return (
    <div className="flex h-full flex-col">
      {/* Summary + metadata — fixed top */}
      <div className="shrink-0 space-y-3 p-4">
        <div className="grid grid-cols-4 gap-3">
          <SummaryCard label="Calls">
            <div className="tabular-nums">{turn.call_count}</div>
          </SummaryCard>
          <SummaryCard label="Total Tokens">
            <div className="flex items-center gap-3 tabular-nums">
              <span className="flex flex-col">
                <span className="text-[10px] text-muted-foreground">in</span>
                <span>{formatNumber(turn.total_input_tokens)}</span>
              </span>
              <span className="flex flex-col">
                <span className="text-[10px] text-muted-foreground">out</span>
                <span>{formatNumber(turn.total_output_tokens)}</span>
              </span>
            </div>
          </SummaryCard>
          <SummaryCard label="Duration">
            <div className="tabular-nums">{formatDuration(turn.duration_ms)}</div>
          </SummaryCard>
          <SummaryCard label="Status / Finish">
            <div className="flex items-center gap-2">
              <TurnStatusBadge status={turn.status} />
              <FinishBadge reason={turn.final_finish_reason} />
            </div>
          </SummaryCard>
        </div>

        <CollapsibleSection title="Metadata" defaultOpen={false}>
          <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
            {metadataRows.map(([label, value]) => (
              <div key={label} className="contents">
                <span className="text-muted-foreground">{label}</span>
                <span className="truncate font-mono text-xs" title={value}>
                  {value}
                </span>
              </div>
            ))}
            <div className="contents">
              <span className="text-muted-foreground">Provider</span>
              <span className="font-mono text-xs">{turn.provider}</span>
            </div>
          </div>
        </CollapsibleSection>
      </div>

      {/* Tabs */}
      <div className="sticky top-0 z-10 flex shrink-0 gap-1 border-b border-border bg-background px-4">
        <TabButton
          active={tab === "input"}
          onClick={() => setTab("input")}
          label="User Input"
        />
        <TabButton
          active={tab === "answer"}
          onClick={() => setTab("answer")}
          label="Final Answer"
        />
        <TabButton
          active={tab === "timeline"}
          onClick={() => setTab("timeline")}
          label="Call Timeline"
          badge={calls.length}
        />
      </div>

      {/* Tab content */}
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        {tab === "input" && (
          turn.user_input
            ? <Markdown text={turn.user_input} />
            : <EmptyTabContent label="user input" />
        )}
        {tab === "answer" && (
          turn.final_answer
            ? <Markdown text={turn.final_answer} />
            : <EmptyTabContent label="final answer" />
        )}
        {tab === "timeline" && <TurnGantt calls={calls} />}
      </div>
    </div>
  )
}

function CallDetailView({
  call,
  detail,
  isLoading,
  isError,
  onBack,
}: {
  call: TurnCallItem
  detail: CallDetail | undefined
  isLoading: boolean
  isError: boolean
  onBack: () => void
}) {
  const requestHeaders = detail ? parseHeaders(detail.request_headers) : []
  const responseHeaders = detail ? parseHeaders(detail.response_headers) : []

  return (
    <div className="flex flex-col">
      <div className="flex items-center gap-2 border-b border-border px-4 py-2">
        <button
          onClick={onBack}
          className="flex items-center gap-1 rounded px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
        >
          <ChevronLeft className="size-3.5" />
          Back to turn
        </button>
        <span className="text-xs text-muted-foreground">
          Call #{call.sequence} of turn
        </span>
      </div>

      {isLoading && !detail ? (
        <div className="flex h-40 items-center justify-center">
          <Loader2 className="size-5 animate-spin text-muted-foreground" />
        </div>
      ) : isError || !detail ? (
        <div className="flex h-40 items-center justify-center text-destructive">
          Failed to load call detail
        </div>
      ) : (
        <>
          <div className="grid grid-cols-4 gap-3 p-4">
            <SummaryCard label="Provider / Model">
              <div>{detail.provider}</div>
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
            </SummaryCard>
          </div>

          {/* Timeline single bar */}
          <div className="px-4 pb-4">
            <CallTimelineBar detail={detail} />
          </div>

          {/* Metadata */}
          <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 px-4 py-3 text-sm">
            {[
              ["ID", detail.id],
              ["Response ID", detail.response_id ?? "—"],
              ["Path", detail.request_path],
              ["Client", `${detail.client_ip}:${detail.client_port}`],
              ["Server", `${detail.server_ip}:${detail.server_port}`],
              ["Stream", detail.is_stream ? "Yes" : "No"],
              ["API Type", detail.api_type],
            ].map(([label, value]) => (
              <div key={label} className="contents">
                <span className="text-muted-foreground">{label}</span>
                <span className="truncate font-mono text-xs" title={String(value)}>
                  {value}
                </span>
              </div>
            ))}
          </div>

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
  )
}

function CallTimelineBar({ detail }: { detail: CallDetail }) {
  const { request_time, complete_time, ttfb_ms, e2e_latency_ms } = detail

  if (!complete_time || !e2e_latency_ms) {
    return (
      <div className="rounded-lg border border-border bg-muted/30 px-4 py-3 text-sm text-muted-foreground">
        Timeline data unavailable
      </div>
    )
  }

  const ttfb = ttfb_ms ?? 0
  const ttfbRatio = ttfb / e2e_latency_ms
  const genRatio = 1 - ttfbRatio

  return (
    <div className="rounded-lg border border-border bg-muted/30 px-4 py-3">
      <div className="mb-2 flex justify-between text-xs text-muted-foreground">
        <span>{formatDateTimeMs(request_time)}</span>
        <span>{formatDateTimeMs(complete_time)}</span>
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
            Gen {formatMs(e2e_latency_ms - ttfb)}
          </div>
        )}
      </div>
    </div>
  )
}

export function TurnDetailPanel({ id, onClose }: Props) {
  const { data: turn, isLoading: loadingTurn, isError: errorTurn } = useTurnDetail(id)
  const { data: calls = [], isLoading: loadingCalls } = useTurnCalls(id)

  const [selectedCallId, setSelectedCallId] = useState<string | null>(null)
  const selectedCall = calls.find((c) => c.id === selectedCallId) ?? null
  const { data: callDetail, isLoading: loadingCallDetail, isError: errorCallDetail } =
    useRequestDetail(selectedCallId)

  return (
    <>
      {/* Backdrop */}
      <div className="fixed inset-0 z-40 bg-black/20" onClick={onClose} />

      {/* Panel */}
      <div className="fixed top-0 right-0 z-50 flex h-full w-[92%] min-w-[720px] flex-col border-l border-border bg-background shadow-xl animate-in slide-in-from-right duration-200">
        {loadingTurn && !turn ? (
          <div className="flex flex-1 items-center justify-center">
            <Loader2 className="size-5 animate-spin text-muted-foreground" />
          </div>
        ) : errorTurn || !turn ? (
          <div className="flex flex-1 flex-col items-center justify-center gap-4 text-destructive">
            <span>Failed to load turn detail</span>
            <button
              onClick={onClose}
              className="rounded border border-border px-3 py-1 text-sm text-muted-foreground hover:bg-muted"
            >
              Close
            </button>
          </div>
        ) : (
          <div className="flex flex-1 overflow-hidden">
            {/* Left panel — call list */}
            <aside className="flex w-[280px] shrink-0 flex-col border-r border-border">
              <div className="shrink-0 border-b border-border px-3 py-3">
                <button
                  onClick={() => setSelectedCallId(null)}
                  className={cn(
                    "block w-full text-left text-sm font-semibold transition-colors",
                    selectedCallId
                      ? "text-muted-foreground hover:text-foreground"
                      : "text-foreground",
                  )}
                >
                  Agent Turn
                </button>
                <div
                  className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground"
                  title={turn.turn_id}
                >
                  {turn.turn_id}
                </div>
              </div>
              <div className="flex-1 overflow-auto p-2">
                {loadingCalls && calls.length === 0 ? (
                  <div className="flex h-20 items-center justify-center">
                    <Loader2 className="size-4 animate-spin text-muted-foreground" />
                  </div>
                ) : calls.length === 0 ? (
                  <div className="px-3 py-6 text-center text-xs text-muted-foreground">
                    No calls
                  </div>
                ) : (
                  <div className="flex flex-col gap-1.5">
                    {calls.map((call) => (
                      <CallCard
                        key={call.id}
                        call={call}
                        selected={call.id === selectedCallId}
                        onClick={() => setSelectedCallId(call.id)}
                      />
                    ))}
                  </div>
                )}
              </div>
            </aside>

            {/* Right panel */}
            <section className="flex flex-1 flex-col overflow-hidden">
              <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">
                <h2 className="text-sm font-semibold">
                  {selectedCall ? "Call Detail" : "Turn Detail"}
                </h2>
                <button
                  onClick={onClose}
                  className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                >
                  <X className="size-4" />
                </button>
              </div>

              {selectedCall ? (
                <div className="flex-1 overflow-y-auto">
                  <CallDetailView
                    call={selectedCall}
                    detail={callDetail}
                    isLoading={loadingCallDetail}
                    isError={errorCallDetail}
                    onBack={() => setSelectedCallId(null)}
                  />
                </div>
              ) : (
                <div className="flex min-h-0 flex-1 flex-col">
                  <TurnDetailView turn={turn} calls={calls} />
                </div>
              )}
            </section>
          </div>
        )}
      </div>
    </>
  )
}
