import { useMemo, useState } from "react"
import { ArrowUpDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { useServices } from "@/hooks/use-services"
import type { ServiceRow } from "@/types/api"

type SortKey =
  | "endpoint"
  | "call_count"
  | "error_rate"
  | "stream_pct"
  | "ttft_avg_ms"
  | "ttft_p95_ms"
  | "e2e_avg_ms"
  | "e2e_p95_ms"
  | "total_input_tokens"
  | "total_output_tokens"
  | "last_seen_ms"
type SortOrder = "asc" | "desc"

function errorRate(s: ServiceRow): number {
  return s.call_count > 0 ? (s.error_count / s.call_count) * 100 : 0
}

function streamPct(s: ServiceRow): number {
  return s.call_count > 0 ? (s.stream_count / s.call_count) * 100 : 0
}

function getSortValue(s: ServiceRow, key: SortKey): number | string {
  if (key === "endpoint") return `${s.server_ip}:${s.server_port}`
  if (key === "error_rate") return errorRate(s)
  if (key === "stream_pct") return streamPct(s)
  return (s[key as keyof ServiceRow] as number) ?? 0
}

/// Color theme per app — picked so the most common production
/// surfaces (vllm/litellm) read at a glance. Falls back to muted gray
/// for unknown so the absence of detection is visually quiet.
const APP_BADGE_STYLE: Record<string, string> = {
  vllm: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  sglang: "bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300",
  ollama: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
  llamacpp: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
  litellm: "bg-pink-100 text-pink-800 dark:bg-pink-900/40 dark:text-pink-300",
  openai: "bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300",
  anthropic: "bg-orange-100 text-orange-800 dark:bg-orange-900/40 dark:text-orange-300",
  gemini: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
}

function AppBadge({ app, serverHeader }: { app: string | null; serverHeader: string | null }) {
  if (!app) {
    return (
      <span
        className="rounded bg-muted/50 px-1.5 py-0.5 text-[10px] text-muted-foreground"
        title={serverHeader ? `Server: ${serverHeader}` : "No identifying signal"}
      >
        unknown
      </span>
    )
  }
  const cls =
    APP_BADGE_STYLE[app] ??
    "bg-slate-200 text-slate-800 dark:bg-slate-700/60 dark:text-slate-200"
  const title = serverHeader
    ? `Server: ${serverHeader}`
    : `Identified as ${app} (no Server header sample)`
  return (
    <span
      className={cn("rounded px-1.5 py-0.5 text-[10px] font-medium", cls)}
      title={title}
    >
      {app}
    </span>
  )
}

function formatAgo(ms: number): string {
  const diff = Date.now() - ms
  if (diff < 0) return "just now"
  const s = Math.floor(diff / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 48) return `${h}h ago`
  const d = Math.floor(h / 24)
  return `${d}d ago`
}

export function ServicesPage() {
  const [sortKey, setSortKey] = useState<SortKey>("call_count")
  const [sortOrder, setSortOrder] = useState<SortOrder>("desc")
  const { data, isLoading } = useServices({
    sortBy:
      // Sort interactively in JS — the server-side sort_by accepts
      // matching column names but we keep client-side sorting for
      // responsive header clicks without refetching.
      "call_count",
    sortOrder: "desc",
  })

  const services = useMemo(() => data?.services ?? [], [data])

  const sorted = useMemo(() => {
    const arr = [...services]
    arr.sort((a, b) => {
      const av = getSortValue(a, sortKey)
      const bv = getSortValue(b, sortKey)
      if (typeof av === "string" && typeof bv === "string") {
        return sortOrder === "asc" ? av.localeCompare(bv) : bv.localeCompare(av)
      }
      return sortOrder === "asc"
        ? (av as number) - (bv as number)
        : (bv as number) - (av as number)
    })
    return arr
  }, [services, sortKey, sortOrder])

  function handleSort(key: SortKey) {
    if (key === sortKey) {
      setSortOrder(sortOrder === "asc" ? "desc" : "asc")
    } else {
      setSortKey(key)
      setSortOrder("desc")
    }
  }

  function SortHeader({
    label,
    field,
    align,
  }: {
    label: string
    field: SortKey
    align?: "left" | "right"
  }) {
    const active = sortKey === field
    return (
      <button
        className={cn(
          "inline-flex items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground",
          align === "right" && "justify-end",
        )}
        onClick={() => handleSort(field)}
      >
        {label}
        <ArrowUpDown
          className={`size-3 ${active ? "text-foreground" : "opacity-40"}`}
        />
      </button>
    )
  }

  return (
    <div className="flex flex-col gap-4 p-4">
      <div className="rounded-lg border border-border bg-card">
        <div className="overflow-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-border">
                <th className="px-4 py-3 text-left">
                  <SortHeader label="Endpoint" field="endpoint" align="left" />
                </th>
                <th className="px-3 py-3 text-left text-xs font-medium text-muted-foreground">
                  App
                </th>
                <th className="px-3 py-3 text-left text-xs font-medium text-muted-foreground">
                  Models
                </th>
                <th className="px-3 py-3 text-left text-xs font-medium text-muted-foreground">
                  Wire API
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="Calls" field="call_count" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="Stream %" field="stream_pct" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="Error %" field="error_rate" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="TTFT avg" field="ttft_avg_ms" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="TTFT p95" field="ttft_p95_ms" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="E2E avg" field="e2e_avg_ms" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="E2E p95" field="e2e_p95_ms" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="In Tokens" field="total_input_tokens" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="Out Tokens" field="total_output_tokens" align="right" />
                </th>
                <th className="px-3 py-3 text-right">
                  <SortHeader label="Last seen" field="last_seen_ms" align="right" />
                </th>
              </tr>
            </thead>
            <tbody>
              {isLoading && services.length === 0 ? (
                <tr>
                  <td colSpan={14} className="py-12 text-center text-muted-foreground">
                    Loading…
                  </td>
                </tr>
              ) : sorted.length === 0 ? (
                <tr>
                  <td colSpan={14} className="py-12 text-center text-muted-foreground">
                    No services found in selected time range
                  </td>
                </tr>
              ) : (
                sorted.map((s) => {
                  const err = errorRate(s)
                  const key = `${s.server_ip}:${s.server_port}`
                  return (
                    <tr key={key} className="border-b border-border/30 hover:bg-muted/30">
                      <td className="px-4 py-2.5 font-mono text-xs">
                        <span className="font-medium">{s.server_ip}</span>
                        <span className="text-muted-foreground">:{s.server_port}</span>
                      </td>
                      <td className="px-3 py-2.5">
                        <AppBadge app={s.app} serverHeader={s.server_header} />
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex flex-wrap gap-1">
                          {s.models.slice(0, 4).map((m) => (
                            <span
                              key={m}
                              className="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-foreground"
                              title={m}
                            >
                              {m.length > 24 ? `${m.slice(0, 22)}…` : m}
                            </span>
                          ))}
                          {s.models.length > 4 && (
                            <span
                              className="rounded border border-dashed border-border px-1.5 py-0.5 text-[10px] text-muted-foreground"
                              title={s.models.slice(4).join(", ")}
                            >
                              +{s.models.length - 4} more
                            </span>
                          )}
                        </div>
                      </td>
                      <td className="px-3 py-2.5 text-xs text-muted-foreground">
                        {s.wire_apis.join(", ")}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {formatNumber(s.call_count)}
                      </td>
                      <td
                        className="px-3 py-2.5 text-right tabular-nums text-muted-foreground"
                        title={`${formatNumber(s.stream_count)} streaming / ${formatNumber(s.call_count)} total`}
                      >
                        {s.stream_count > 0 ? `${streamPct(s).toFixed(0)}%` : "—"}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        <span
                          className={
                            err > 5 ? "text-red-500" : err > 1 ? "text-amber-500" : ""
                          }
                        >
                          {err.toFixed(1)}%
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {formatMs(s.ttft_avg_ms)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {formatMs(s.ttft_p95_ms)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {formatMs(s.e2e_avg_ms)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {formatMs(s.e2e_p95_ms)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                        {formatNumber(s.total_input_tokens)}
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                        {formatNumber(s.total_output_tokens)}
                      </td>
                      <td
                        className="px-3 py-2.5 text-right text-xs text-muted-foreground tabular-nums"
                        title={new Date(s.last_seen_ms).toLocaleString()}
                      >
                        {formatAgo(s.last_seen_ms)}
                      </td>
                    </tr>
                  )
                })
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}
