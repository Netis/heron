import { useCallback } from "react"
import { ArrowUpDown, ArrowUp, ArrowDown, ChevronLeft, ChevronRight, Loader2, Filter } from "lucide-react"
import { cn } from "@/lib/utils"
import { useAgentTurns } from "@/hooks/use-agent-turns"
import { useSearchParamState } from "@/hooks/use-search-param-state"
import { formatDateTimeMs, formatNumber, formatDuration } from "@/lib/format"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import { ProxyBadge } from "@/components/ui/proxy-badge"
import { AgentTurnDetailPanel } from "./agent-turn-detail-panel"
import type { AgentTurnListItem } from "@/types/api"

const STATUS_OPTIONS = ["in_progress", "complete", "incomplete"]
const AGENT_KIND_OPTIONS = ["claude-cli", "codex-cli", "generic"]

const PAGE_SIZES = [20, 50, 100] as const

// Identity columns (Agent / Client) sit immediately after Time; coarse
// shape (Calls / Status) and per-turn token counters (In / Out) follow
// — that's the order operators reach for first when triaging a turn.
// Less-frequently-scanned dimensions (Model / Wire API / Server) and
// the long preview column trail.
const columns = [
  { key: "start_time", label: "Time", width: "w-[210px]", sortable: true, align: "left" as const },
  { key: "agent_kind", label: "Agent", width: "w-[100px]", sortable: false, align: "left" as const },
  { key: "client_ip", label: "Client", width: "w-[130px]", sortable: false, align: "left" as const },
  { key: "call_count", label: "Calls", width: "w-[60px]", sortable: true, align: "right" as const },
  { key: "status", label: "Status", width: "w-[100px]", sortable: false, align: "left" as const },
  { key: "total_input_tokens", label: "In", width: "w-[70px]", sortable: true, align: "right" as const },
  { key: "total_output_tokens", label: "Out", width: "w-[70px]", sortable: true, align: "right" as const },
  { key: "primary_model", label: "Model", width: "w-[180px]", sortable: false, align: "left" as const },
  { key: "wire_api", label: "Wire API", width: "w-[120px]", sortable: false, align: "left" as const },
  { key: "server_ip", label: "Server", width: "w-[130px]", sortable: false, align: "left" as const },
  { key: "duration_ms", label: "Duration", width: "w-[90px]", sortable: true, align: "right" as const },
  { key: "preview", label: "User Input", width: "", sortable: false, align: "left" as const },
] as const

function SortIcon({ column, sortBy, sortOrder }: { column: string; sortBy: string; sortOrder: string }) {
  if (column !== sortBy) return <ArrowUpDown className="size-3 opacity-0 group-hover:opacity-50" />
  return sortOrder === "asc" ? <ArrowUp className="size-3" /> : <ArrowDown className="size-3" />
}

function CellValue({ item, column }: { item: AgentTurnListItem; column: (typeof columns)[number]["key"] }) {
  switch (column) {
    case "start_time":
      return (
        <span className="inline-flex items-center gap-2">
          <span className="tabular-nums">{formatDateTimeMs(item.start_time)}</span>
          <ProxyBadge item={item} />
        </span>
      )
    case "wire_api":
      return (
        <span className="truncate" title={item.wire_api}>
          {item.wire_api}
        </span>
      )
    case "primary_model":
      return (
        <span className="truncate" title={item.primary_model ?? undefined}>
          {item.primary_model ?? "—"}
        </span>
      )
    case "agent_kind":
      return (
        <span className="truncate" title={item.agent_kind}>
          {item.agent_kind}
        </span>
      )
    case "client_ip":
      return <span className="truncate font-mono text-xs">{item.client_ip}</span>
    case "server_ip":
      return <span className="truncate font-mono text-xs">{item.server_ip}</span>
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

export function AgentTurnsPage() {
  const [pageStr, setPageStr] = useSearchParamState("page", "1")
  const [pageSizeStr, setPageSizeStr] = useSearchParamState("page_size", "50")
  const [sortBy, setSortBy] = useSearchParamState("sort", "start_time")
  const [sortOrder, setSortOrder] = useSearchParamState("order", "desc")
  const [statusStr, setStatusStr] = useSearchParamState("status", "")
  const [agentKindStr, setAgentKindStr] = useSearchParamState("agent_kind", "")
  const [clientIpStr, setClientIpStr] = useSearchParamState("client_ip", "")
  // Default off — the user wanted the folded view as the primary
  // experience. URL serialization keeps "show hops" sticky on a
  // shared link.
  const [showHopsStr, setShowHopsStr] = useSearchParamState("show_hops", "")
  const includeProxyHops = showHopsStr === "1"

  const page = Number(pageStr) || 1
  const pageSize = Number(pageSizeStr) || 50
  const statusFilter = statusStr ? statusStr.split(",") : []
  const agentKindFilter = agentKindStr ? agentKindStr.split(",") : []

  const [selectedId, setSelectedId] = useSearchParamState("selected", "")

  const { data, isLoading, isError, error } = useAgentTurns({
    page,
    pageSize,
    sortBy,
    sortOrder: sortOrder as "asc" | "desc",
    status: statusStr || undefined,
    agentKind: agentKindStr || undefined,
    clientIp: clientIpStr || undefined,
    includeProxyHops,
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
        <FilterDropdown
          label="Agent kind"
          options={AGENT_KIND_OPTIONS}
          selected={agentKindFilter}
          onChange={(v) => {
            setAgentKindStr(v.join(","))
            setPageStr("1")
          }}
        />
        <input
          value={clientIpStr}
          onChange={(e) => { setClientIpStr(e.target.value); setPageStr("1") }}
          placeholder="Client IP (CSV)"
          className="w-[180px] rounded-lg border border-border bg-background px-2.5 py-1.5 text-xs placeholder:text-muted-foreground focus:border-foreground/20 focus:outline-none"
        />
        <label
          className="inline-flex cursor-pointer select-none items-center gap-1.5 rounded-lg border border-border px-2.5 py-1.5 text-xs hover:bg-muted"
          title="Show the upstream/mirror leg of llmproxy duplicates (hidden by default)"
        >
          <input
            type="checkbox"
            checked={includeProxyHops}
            onChange={(e) => {
              setShowHopsStr(e.target.checked ? "1" : "")
              setPageStr("1")
            }}
            className="size-3"
          />
          Show proxy hops
        </label>
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
                  Failed to load agent turns: {error?.message}
                </td>
              </tr>
            ) : items.length === 0 ? (
              <tr>
                <td colSpan={columns.length} className="py-20 text-center text-muted-foreground">
                  No agent turns found in the selected time range
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
        <AgentTurnDetailPanel id={selectedId} onClose={() => setSelectedId("")} />
      )}
    </div>
  )
}
