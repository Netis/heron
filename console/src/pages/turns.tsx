import { useState, useCallback } from "react"
import { ArrowUpDown, ArrowUp, ArrowDown, ChevronLeft, ChevronRight, Loader2, Filter } from "lucide-react"
import { cn } from "@/lib/utils"
import { useTurns } from "@/hooks/use-turns"
import { useSearchParamState } from "@/hooks/use-search-param-state"
import { formatTime, formatNumber, formatDuration } from "@/lib/format"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import { TurnDetailPanel } from "./turn-detail-panel"
import type { TurnListItem } from "@/types/api"

const STATUS_OPTIONS = ["success", "error", "incomplete", "in_progress", "timeout", "cancelled"]

const PAGE_SIZES = [20, 50, 100] as const

const columns = [
  { key: "start_time", label: "Time", width: "w-[140px]", sortable: true, align: "left" as const },
  { key: "provider", label: "Provider", width: "w-[100px]", sortable: false, align: "left" as const },
  { key: "primary_model", label: "Model", width: "w-[180px]", sortable: false, align: "left" as const },
  { key: "client_kind", label: "Client", width: "w-[100px]", sortable: false, align: "left" as const },
  { key: "status", label: "Status", width: "w-[100px]", sortable: false, align: "left" as const },
  { key: "call_count", label: "Calls", width: "w-[60px]", sortable: true, align: "right" as const },
  { key: "total_input_tokens", label: "In", width: "w-[70px]", sortable: true, align: "right" as const },
  { key: "total_output_tokens", label: "Out", width: "w-[70px]", sortable: true, align: "right" as const },
  { key: "duration_ms", label: "Duration", width: "w-[90px]", sortable: true, align: "right" as const },
  { key: "preview", label: "User Input", width: "", sortable: false, align: "left" as const },
] as const

function SortIcon({ column, sortBy, sortOrder }: { column: string; sortBy: string; sortOrder: string }) {
  if (column !== sortBy) return <ArrowUpDown className="size-3 opacity-0 group-hover:opacity-50" />
  return sortOrder === "asc" ? <ArrowUp className="size-3" /> : <ArrowDown className="size-3" />
}

function CellValue({ item, column }: { item: TurnListItem; column: (typeof columns)[number]["key"] }) {
  switch (column) {
    case "start_time":
      return <span className="tabular-nums">{formatTime(item.start_time)}</span>
    case "provider":
      return (
        <span className="truncate" title={item.provider}>
          {item.provider}
        </span>
      )
    case "primary_model":
      return (
        <span className="truncate" title={item.primary_model ?? undefined}>
          {item.primary_model ?? "—"}
        </span>
      )
    case "client_kind":
      return (
        <span className="truncate" title={item.client_kind}>
          {item.client_kind}
        </span>
      )
    case "status":
      return <TurnStatusBadge status={item.status} />
    case "call_count":
      return <span className="tabular-nums">{item.call_count}</span>
    case "total_input_tokens":
      return <span className="tabular-nums">{formatNumber(item.total_input_tokens)}</span>
    case "total_output_tokens":
      return <span className="tabular-nums">{formatNumber(item.total_output_tokens)}</span>
    case "duration_ms":
      return <span className="tabular-nums">{formatDuration(item.duration_ms)}</span>
    case "preview":
      return (
        <span className="truncate text-muted-foreground" title={item.user_input_preview ?? undefined}>
          {item.user_input_preview ?? "—"}
        </span>
      )
  }
}

export function TurnsPage() {
  const [pageStr, setPageStr] = useSearchParamState("page", "1")
  const [pageSizeStr, setPageSizeStr] = useSearchParamState("page_size", "50")
  const [sortBy, setSortBy] = useSearchParamState("sort", "start_time")
  const [sortOrder, setSortOrder] = useSearchParamState("order", "desc")
  const [statusStr, setStatusStr] = useSearchParamState("status", "")

  const page = Number(pageStr) || 1
  const pageSize = Number(pageSizeStr) || 50
  const statusFilter = statusStr ? statusStr.split(",") : []

  const [selectedId, setSelectedId] = useState<string | null>(null)

  const { data, isLoading, isError, error } = useTurns({
    page,
    pageSize,
    sortBy,
    sortOrder: sortOrder as "asc" | "desc",
    status: statusStr || undefined,
  })

  const items = data?.items ?? []
  const total = data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  const rangeStart = total === 0 ? 0 : (page - 1) * pageSize + 1
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

  return (
    <div className="relative flex h-full flex-col">
      {/* Page-specific filters */}
      <div className="flex shrink-0 items-center gap-2 border-b border-border px-4 py-2">
        <Filter className="size-3.5 text-muted-foreground" />
        <span className="text-xs text-muted-foreground">Filters:</span>
        <FilterDropdown
          label="Status"
          options={STATUS_OPTIONS}
          selected={statusFilter}
          onChange={(v) => {
            setStatusStr(v.join(","))
            setPageStr("1")
          }}
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
                    "group px-3 py-2 text-xs font-medium text-muted-foreground select-none",
                    col.width,
                    col.align === "right" ? "text-right" : "text-left",
                    col.sortable && "cursor-pointer",
                  )}
                >
                  <span
                    className={cn(
                      "inline-flex items-center gap-1",
                      col.align === "right" && "flex-row-reverse",
                    )}
                  >
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
                  Failed to load turns: {error?.message}
                </td>
              </tr>
            ) : items.length === 0 ? (
              <tr>
                <td colSpan={columns.length} className="py-20 text-center text-muted-foreground">
                  No turns found in the selected time range
                </td>
              </tr>
            ) : (
              items.map((item) => (
                <tr
                  key={item.turn_id}
                  onClick={() => setSelectedId(item.turn_id)}
                  className={cn(
                    "cursor-pointer border-b border-border/50 transition-colors hover:bg-muted/50",
                    selectedId === item.turn_id && "bg-muted",
                  )}
                >
                  {columns.map((col) => (
                    <td
                      key={col.key}
                      className={cn(
                        "px-3 py-1.5",
                        col.width,
                        col.align === "right" && "text-right",
                        col.key === "preview" && "max-w-0",
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
        <TurnDetailPanel id={selectedId} onClose={() => setSelectedId(null)} />
      )}
    </div>
  )
}
