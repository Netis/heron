import { useCallback } from "react"
import {
  ArrowUpDown,
  ArrowUp,
  ArrowDown,
  ChevronLeft,
  ChevronRight,
  Loader2,
  Filter,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { useHttpExchanges } from "@/hooks/use-http-exchanges"
import { useSearchParamState } from "@/hooks/use-search-param-state"
import { formatDateTimeMs, formatMs } from "@/lib/format"
import { StatusBadge } from "@/components/ui/status-badge"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import { HttpExchangeDetailPanel } from "./http-exchange-detail-panel"
import type { HttpExchangeListItem } from "@/types/api"

const METHOD_OPTIONS = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"]
const STATUS_OPTIONS = ["200", "201", "204", "301", "302", "400", "401", "403", "404", "429", "500", "502", "503"]
const SSE_OPTIONS = [
  { value: "", label: "Any" },
  { value: "true", label: "SSE only" },
  { value: "false", label: "Non-SSE" },
]

const PAGE_SIZES = [20, 50, 100] as const

// Only the columns with real SQL-level sort support go through handleSort.
// Everything else is display-only.
const SORTABLE: Record<string, true> = {
  request_time: true,
  status: true,
  duration_ms: true,
}

const columns = [
  { key: "request_time", label: "Time", width: "w-[210px]" },
  { key: "method", label: "Method", width: "w-[80px]" },
  { key: "uri", label: "URI", width: "" },
  { key: "client_ip", label: "Client", width: "w-[140px]" },
  { key: "server", label: "Server", width: "w-[180px]" },
  { key: "status", label: "Status", width: "w-[72px]" },
  { key: "is_sse", label: "SSE", width: "w-[52px]" },
  { key: "duration_ms", label: "Duration", width: "w-[92px]" },
] as const

type SortKey = (typeof columns)[number]["key"]

function SortIcon({
  column,
  sortBy,
  sortOrder,
}: {
  column: string
  sortBy: string
  sortOrder: string
}) {
  if (!SORTABLE[column]) return null
  if (column !== sortBy)
    return <ArrowUpDown className="size-3 opacity-0 group-hover:opacity-50" />
  return sortOrder === "asc" ? <ArrowUp className="size-3" /> : <ArrowDown className="size-3" />
}

function MethodBadge({ method }: { method: string }) {
  const tone =
    method === "GET"
      ? "bg-emerald-500/15 text-emerald-700 dark:text-emerald-300"
      : method === "POST"
        ? "bg-blue-500/15 text-blue-700 dark:text-blue-300"
        : method === "DELETE"
          ? "bg-red-500/15 text-red-700 dark:text-red-300"
          : method === "PUT" || method === "PATCH"
            ? "bg-amber-500/15 text-amber-700 dark:text-amber-300"
            : "bg-muted text-muted-foreground"
  return (
    <span className={cn("rounded px-1.5 py-0.5 font-mono text-[10px] font-medium", tone)}>
      {method}
    </span>
  )
}

function CellValue({ item, column }: { item: HttpExchangeListItem; column: SortKey }) {
  switch (column) {
    case "request_time":
      return <span className="tabular-nums">{formatDateTimeMs(item.request_time)}</span>
    case "method":
      return <MethodBadge method={item.method} />
    case "uri":
      return (
        <span className="block truncate font-mono text-xs" title={item.uri}>
          {item.uri}
        </span>
      )
    case "client_ip":
      return <span className="truncate font-mono text-xs">{item.client_ip}</span>
    case "server":
      return (
        <span className="truncate font-mono text-xs">
          {item.server_ip}:{item.server_port}
        </span>
      )
    case "status":
      return <StatusBadge status={item.status} />
    case "is_sse":
      return (
        <span className={item.is_sse ? "text-blue-500" : "text-muted-foreground"}>
          {item.is_sse ? "⚡" : "—"}
        </span>
      )
    case "duration_ms":
      return <span className="tabular-nums">{formatMs(item.duration_ms)}</span>
  }
}

export function HttpExchangesPage() {
  const [pageStr, setPageStr] = useSearchParamState("page", "1")
  const [pageSizeStr, setPageSizeStr] = useSearchParamState("page_size", "50")
  const [sortBy, setSortBy] = useSearchParamState("sort", "request_time")
  const [sortOrder, setSortOrder] = useSearchParamState("order", "desc")
  const [methodStr, setMethodStr] = useSearchParamState("method", "")
  const [statusStr, setStatusStr] = useSearchParamState("status", "")
  const [sseStr, setSseStr] = useSearchParamState("sse", "")
  const [clientIpStr, setClientIpStr] = useSearchParamState("client_ip", "")
  const [uriStr, setUriStr] = useSearchParamState("uri", "")

  const page = Number(pageStr) || 1
  const pageSize = Number(pageSizeStr) || 50
  const methodFilter = methodStr ? methodStr.split(",") : []
  const statusFilter = statusStr ? statusStr.split(",") : []
  const isSse = sseStr === "true" ? true : sseStr === "false" ? false : undefined

  const [selectedId, setSelectedId] = useSearchParamState("selected", "")
  // Anchor (unix seconds) shared alongside `?selected` so a recipient who
  // opens this URL with a stale relative preset still lands on the
  // exchange's window — see use-url-sync.ts for the override logic.
  const [, setSelectedAt] = useSearchParamState("selected_at", "")

  const { data, isLoading, isError, error } = useHttpExchanges({
    page,
    pageSize,
    sortBy,
    sortOrder: sortOrder as "asc" | "desc",
    method: methodStr || undefined,
    status: statusStr || undefined,
    clientIp: clientIpStr || undefined,
    uri: uriStr || undefined,
    isSse,
  })

  const items = data?.items ?? []
  const total = data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  const rangeStart = (page - 1) * pageSize + 1
  const rangeEnd = Math.min(page * pageSize, total)

  const handleSort = useCallback(
    (key: string) => {
      if (!SORTABLE[key]) return
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

  // Index derived from id so the selection survives URL paste / refresh:
  // we own only one source of truth (the URL), and prev/next still works
  // as long as the selected id is on the current page.
  const selectedIndex = selectedId
    ? items.findIndex((i) => i.id === selectedId)
    : -1

  const selectItemById = useCallback(
    (id: string) => {
      const item = items.find((i) => i.id === id)
      setSelectedId(id)
      // request_time is unix ms — convert to seconds for the anchor.
      setSelectedAt(item ? String(Math.floor(item.request_time / 1000)) : "")
    },
    [items, setSelectedId, setSelectedAt],
  )

  const handleRowClick = useCallback(
    (id: string, _index: number) => {
      selectItemById(id)
    },
    [selectItemById],
  )

  const handleNavigate = useCallback(
    (direction: "prev" | "next") => {
      const newIndex = direction === "prev" ? selectedIndex - 1 : selectedIndex + 1
      if (newIndex >= 0 && newIndex < items.length) {
        selectItemById(items[newIndex].id)
      }
    },
    [selectedIndex, items, selectItemById],
  )

  const handleClose = useCallback(() => {
    setSelectedId("")
    setSelectedAt("")
  }, [setSelectedId, setSelectedAt])

  return (
    <div className="relative flex h-full flex-col">
      {/* Page-specific filters */}
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-4 py-2">
        <Filter className="size-3.5 text-muted-foreground" />
        <span className="text-xs text-muted-foreground">Filters:</span>
        <FilterDropdown
          label="Method"
          options={METHOD_OPTIONS}
          selected={methodFilter}
          onChange={(v) => {
            setMethodStr(v.join(","))
            setPageStr("1")
          }}
        />
        <FilterDropdown
          label="Status"
          options={STATUS_OPTIONS}
          selected={statusFilter}
          onChange={(v) => {
            setStatusStr(v.join(","))
            setPageStr("1")
          }}
        />
        <select
          value={sseStr}
          onChange={(e) => {
            setSseStr(e.target.value)
            setPageStr("1")
          }}
          className="rounded border border-border bg-background px-1.5 py-0.5 text-xs"
        >
          {SSE_OPTIONS.map((o) => (
            <option key={o.value} value={o.value}>
              SSE: {o.label}
            </option>
          ))}
        </select>
        <input
          value={clientIpStr}
          onChange={(e) => {
            setClientIpStr(e.target.value)
            setPageStr("1")
          }}
          placeholder="Client IP (CSV)"
          className="w-[180px] rounded-lg border border-border bg-background px-2.5 py-1.5 text-xs placeholder:text-muted-foreground focus:border-foreground/20 focus:outline-none"
        />
        <input
          value={uriStr}
          onChange={(e) => {
            setUriStr(e.target.value)
            setPageStr("1")
          }}
          placeholder="URI contains…"
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
                  onClick={() => handleSort(col.key)}
                  className={cn(
                    "group px-3 py-2 text-left text-xs font-medium text-muted-foreground select-none",
                    col.width,
                    SORTABLE[col.key] && "cursor-pointer",
                    col.key === "duration_ms" && "text-right",
                  )}
                >
                  <span className="inline-flex items-center gap-1">
                    {col.label}
                    <SortIcon column={col.key} sortBy={sortBy} sortOrder={sortOrder} />
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
                  Failed to load HTTP logs: {error?.message}
                </td>
              </tr>
            ) : items.length === 0 ? (
              <tr>
                <td colSpan={columns.length} className="py-20 text-center text-muted-foreground">
                  No HTTP logs in the selected time range
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
                        col.key === "duration_ms" && "text-right",
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
        <HttpExchangeDetailPanel
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
