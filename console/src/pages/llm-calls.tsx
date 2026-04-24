import { useState, useCallback } from "react"
import { ArrowUpDown, ArrowUp, ArrowDown, ChevronLeft, ChevronRight, Loader2, Filter } from "lucide-react"
import { cn } from "@/lib/utils"
import { useLlmCalls } from "@/hooks/use-llm-calls"
import { useSearchParamState } from "@/hooks/use-search-param-state"
import { formatTime, formatMs, formatNumber } from "@/lib/format"
import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import { LlmCallDetailPanel } from "./llm-call-detail-panel"
import type { LlmCallListItem } from "@/types/api"

const STATUS_OPTIONS = ["200", "400", "401", "403", "404", "429", "500", "502", "503"]
const FINISH_OPTIONS = ["complete", "stop", "length", "tool_use", "error", "cancelled"]

const PAGE_SIZES = [20, 50, 100] as const

const columns = [
  { key: "request_time", label: "Time", width: "w-[160px]", sortable: true },
  { key: "wire_api", label: "Wire API", width: "w-[110px]", sortable: false },
  { key: "model", label: "Model", width: "w-[140px]", sortable: false },
  { key: "client_ip", label: "Client", width: "w-[130px]", sortable: false },
  { key: "request_path", label: "Path", width: "", sortable: false },
  { key: "status_code", label: "Status", width: "w-[52px]", sortable: true },
  { key: "is_stream", label: "S", width: "w-[32px]", sortable: false },
  { key: "finish_reason", label: "Finish", width: "w-[72px]", sortable: false },
  { key: "ttft_ms", label: "TTFT", width: "w-[72px]", sortable: true },
  { key: "e2e_latency_ms", label: "E2E", width: "w-[72px]", sortable: true },
  { key: "input_tokens", label: "In", width: "w-[56px]", sortable: true },
  { key: "output_tokens", label: "Out", width: "w-[56px]", sortable: true },
] as const

type SortKey = (typeof columns)[number]["key"]

function SortIcon({ column, sortBy, sortOrder }: { column: string; sortBy: string; sortOrder: string }) {
  if (column !== sortBy) return <ArrowUpDown className="size-3 opacity-0 group-hover:opacity-50" />
  return sortOrder === "asc" ? (
    <ArrowUp className="size-3" />
  ) : (
    <ArrowDown className="size-3" />
  )
}

function CellValue({ item, column }: { item: LlmCallListItem; column: SortKey }) {
  switch (column) {
    case "request_time":
      return <span className="tabular-nums">{formatTime(item.request_time)}</span>
    case "wire_api":
      return <span className="truncate">{item.wire_api}</span>
    case "model":
      return (
        <span className="truncate" title={item.model}>
          {item.model}
        </span>
      )
    case "client_ip":
      return <span className="truncate font-mono text-xs">{item.client_ip}</span>
    case "request_path":
      return (
        <span className="block truncate font-mono text-xs" title={item.request_path}>
          {item.request_path}
        </span>
      )
    case "status_code":
      return <StatusBadge status={item.status_code} />
    case "is_stream":
      return (
        <span className={item.is_stream ? "text-blue-500" : "text-muted-foreground"}>
          {item.is_stream ? "\u26A1" : "\u2014"}
        </span>
      )
    case "finish_reason":
      return <FinishBadge reason={item.finish_reason} />
    case "ttft_ms":
      return <span className="tabular-nums">{formatMs(item.ttft_ms)}</span>
    case "e2e_latency_ms":
      return <span className="tabular-nums">{formatMs(item.e2e_latency_ms)}</span>
    case "input_tokens":
      return <span className="tabular-nums">{formatNumber(item.input_tokens)}</span>
    case "output_tokens":
      return <span className="tabular-nums">{formatNumber(item.output_tokens)}</span>
  }
}

export function LlmCallsPage() {
  const [pageStr, setPageStr] = useSearchParamState("page", "1")
  const [pageSizeStr, setPageSizeStr] = useSearchParamState("page_size", "50")
  const [sortBy, setSortBy] = useSearchParamState("sort", "request_time")
  const [sortOrder, setSortOrder] = useSearchParamState("order", "desc")
  const [statusStr, setStatusStr] = useSearchParamState("status", "")
  const [finishStr, setFinishStr] = useSearchParamState("finish", "")
  const [clientIpStr, setClientIpStr] = useSearchParamState("client_ip", "")
  const [pathStr, setPathStr] = useSearchParamState("path", "")
  const [errorsOnlyStr, setErrorsOnlyStr] = useSearchParamState("errors_only", "")

  const page = Number(pageStr) || 1
  const pageSize = Number(pageSizeStr) || 50
  const statusFilter = statusStr ? statusStr.split(",") : []
  const finishFilter = finishStr ? finishStr.split(",") : []
  const errorsOnly = errorsOnlyStr === "true"

  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [selectedIndex, setSelectedIndex] = useState(-1)

  const { data, isLoading, isError, error } = useLlmCalls({
    page,
    pageSize,
    sortBy,
    sortOrder: sortOrder as "asc" | "desc",
    statusCode: statusStr || undefined,
    finishReason: finishStr || undefined,
    clientIp: clientIpStr || undefined,
    requestPath: pathStr || undefined,
    errorsOnly,
  })

  const items = data?.items ?? []
  const total = data?.total ?? 0
  const totalPages = Math.ceil(total / pageSize)
  const rangeStart = (page - 1) * pageSize + 1
  const rangeEnd = Math.min(page * pageSize, total)

  const handleSort = useCallback(
    (key: string, sortable: boolean) => {
      if (!sortable) return
      if (key === sortBy) {
        setSortOrder(sortOrder === "asc" ? "desc" : "asc")
      } else {
        setSortBy(key)
        setSortOrder("desc")
      }
      setPageStr("1")
    },
    [sortBy, sortOrder, setSortBy, setSortOrder, setPageStr],
  )

  const handleRowClick = useCallback((id: string, index: number) => {
    setSelectedId(id)
    setSelectedIndex(index)
  }, [])

  const handleNavigate = useCallback(
    (direction: "prev" | "next") => {
      const newIndex = direction === "prev" ? selectedIndex - 1 : selectedIndex + 1
      if (newIndex >= 0 && newIndex < items.length) {
        setSelectedIndex(newIndex)
        setSelectedId(items[newIndex].id)
      }
    },
    [selectedIndex, items],
  )

  const handleClose = useCallback(() => {
    setSelectedId(null)
    setSelectedIndex(-1)
  }, [])

  return (
    <div className="relative flex h-full flex-col">
      {/* Page-specific filters */}
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-4 py-2">
        <Filter className="size-3.5 text-muted-foreground" />
        <span className="text-xs text-muted-foreground">Filters:</span>
        <button
          onClick={() => {
            setErrorsOnlyStr(errorsOnly ? "" : "true")
            setPageStr("1")
          }}
          className={cn(
            "flex items-center gap-1.5 rounded-lg border px-2.5 py-1.5 text-xs transition-colors hover:bg-muted",
            errorsOnly
              ? "border-foreground/20 bg-muted font-medium"
              : "border-border text-muted-foreground",
          )}
        >
          Errors only
        </button>
        <FilterDropdown
          label="Status"
          options={STATUS_OPTIONS}
          selected={statusFilter}
          onChange={(v) => { setStatusStr(v.join(",")); setPageStr("1") }}
        />
        <FilterDropdown
          label="Finish Reason"
          options={FINISH_OPTIONS}
          selected={finishFilter}
          onChange={(v) => { setFinishStr(v.join(",")); setPageStr("1") }}
        />
        <input
          value={clientIpStr}
          onChange={(e) => { setClientIpStr(e.target.value); setPageStr("1") }}
          placeholder="Client IP (CSV)"
          className="w-[180px] rounded-lg border border-border bg-background px-2.5 py-1.5 text-xs placeholder:text-muted-foreground focus:border-foreground/20 focus:outline-none"
        />
        <input
          value={pathStr}
          onChange={(e) => { setPathStr(e.target.value); setPageStr("1") }}
          placeholder="Path contains…"
          className="w-[220px] rounded-lg border border-border bg-background px-2.5 py-1.5 text-xs placeholder:text-muted-foreground focus:border-foreground/20 focus:outline-none"
        />
      </div>

      {/* Table */}
      <div className="flex-1 overflow-auto">
        <table className="w-full text-sm">
          <thead className="sticky top-0 z-10 bg-background">
            <tr className="border-b border-border">
              {columns.map((col) => (
                <th
                  key={col.key}
                  onClick={() => handleSort(col.key, col.sortable)}
                  className={cn(
                    "group px-3 py-2 text-left text-xs font-medium text-muted-foreground select-none",
                    col.width,
                    col.sortable && "cursor-pointer",
                    (col.key === "ttft_ms" ||
                      col.key === "e2e_latency_ms" ||
                      col.key === "input_tokens" ||
                      col.key === "output_tokens") &&
                      "text-right",
                  )}
                >
                  <span className="inline-flex items-center gap-1">
                    {col.label}
                    {col.sortable && (
                      <SortIcon column={col.key} sortBy={sortBy} sortOrder={sortOrder} />
                    )}
                  </span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {isLoading && items.length === 0 ? (
              <tr>
                <td colSpan={columns.length} className="py-20 text-center text-muted-foreground">
                  <Loader2 className="mx-auto size-5 animate-spin" />
                </td>
              </tr>
            ) : isError ? (
              <tr>
                <td colSpan={columns.length} className="py-20 text-center text-destructive">
                  Failed to load LLM calls: {error?.message}
                </td>
              </tr>
            ) : items.length === 0 ? (
              <tr>
                <td colSpan={columns.length} className="py-20 text-center text-muted-foreground">
                  No LLM calls found in the selected time range
                </td>
              </tr>
            ) : (
              items.map((item, index) => (
                <tr
                  key={item.id}
                  onClick={() => handleRowClick(item.id, index)}
                  className={cn(
                    "cursor-pointer border-b border-border/50 transition-colors hover:bg-muted/50",
                    selectedId === item.id && "bg-muted",
                  )}
                >
                  {columns.map((col) => (
                    <td
                      key={col.key}
                      className={cn(
                        "px-3 py-1.5",
                        col.width,
                        col.key === "request_path" && "max-w-0",
                        (col.key === "ttft_ms" ||
                          col.key === "e2e_latency_ms" ||
                          col.key === "input_tokens" ||
                          col.key === "output_tokens") &&
                          "text-right",
                      )}
                    >
                      <CellValue item={item} column={col.key} />
                    </td>
                  ))}
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>

      {/* Pagination */}
      {total > 0 && (
        <div className="flex shrink-0 items-center justify-between border-t border-border px-4 py-2 text-sm">
          <div className="flex items-center gap-2 text-muted-foreground">
            <span>
              {rangeStart}-{rangeEnd} of {total.toLocaleString()}
            </span>
            <select
              value={pageSize}
              onChange={(e) => {
                setPageSizeStr(e.target.value)
                setPageStr("1")
              }}
              className="rounded border border-border bg-background px-1.5 py-0.5 text-xs"
            >
              {PAGE_SIZES.map((s) => (
                <option key={s} value={s}>
                  {s} / page
                </option>
              ))}
            </select>
          </div>
          <div className="flex items-center gap-1">
            <button
              onClick={() => setPageStr(String(Math.max(1, page - 1)))}
              disabled={page <= 1}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronLeft className="size-4" />
            </button>
            <span className="px-2 tabular-nums text-muted-foreground">
              {page} / {totalPages}
            </span>
            <button
              onClick={() => setPageStr(String(Math.min(totalPages, page + 1)))}
              disabled={page >= totalPages}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronRight className="size-4" />
            </button>
          </div>
        </div>
      )}

      {/* Slide-over detail panel */}
      {selectedId && (
        <LlmCallDetailPanel
          id={selectedId}
          onClose={handleClose}
          onNavigate={handleNavigate}
          hasPrev={selectedIndex > 0}
          hasNext={selectedIndex < items.length - 1}
        />
      )}
    </div>
  )
}
