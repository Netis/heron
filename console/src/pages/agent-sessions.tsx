import { useState } from "react"
import { Link, useSearchParams } from "react-router"
import { Loader2, Filter } from "lucide-react"
import { cn } from "@/lib/utils"
import { useAgentSessions } from "@/hooks/use-agent-sessions"
import { formatNumber, formatRelativeTime, formatDuration } from "@/lib/format"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import { AgentBadge } from "@/components/ui/agent-badge"
import type { SessionListItem } from "@/types/api"

const AGENT_KIND_OPTIONS = ["claude-cli", "codex-cli"]

function SessionRow({ item }: { item: SessionListItem }) {
  const [searchParams] = useSearchParams()
  const qs = searchParams.toString()
  const href = `/agent-sessions/${encodeURIComponent(item.source_id)}/${encodeURIComponent(item.session_id)}${qs ? `?${qs}` : ""}`

  const preview = item.first_user_input_preview ?? "(no user message)"
  const cost = item.total_cost_usd != null ? `$${item.total_cost_usd.toFixed(2)}` : null
  const durationMs = item.last_turn_at - item.first_turn_at

  return (
    <Link
      to={href}
      className="block border-b border-border/50 px-4 py-3 transition-colors hover:bg-muted/40"
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <AgentBadge agentKind={item.agent_kind} />
            <span className="font-mono text-xs text-muted-foreground">
              {item.session_id}
            </span>
          </div>
          <div className="mt-1 truncate text-sm text-foreground">{preview}</div>
          <div className="mt-1 text-xs text-muted-foreground">
            {item.turn_count} turns · {item.call_count} calls ·{" "}
            {formatNumber(item.total_input_tokens + item.total_output_tokens)} tok
            {cost ? ` · ${cost}` : ""}
          </div>
        </div>
        <div className="shrink-0 text-right text-xs text-muted-foreground">
          <div>{formatRelativeTime(item.last_turn_at_in_window)}</div>
          <div className="text-[11px] opacity-70">{formatDuration(durationMs)}</div>
        </div>
      </div>
    </Link>
  )
}

export function AgentSessionsPage() {
  const [agentKindFilter, setAgentKindFilter] = useState<string[]>([])

  const { data, isLoading, isError, error, fetchNextPage, hasNextPage, isFetchingNextPage } =
    useAgentSessions({
      agentKind: agentKindFilter.join(","),
    })

  const items: SessionListItem[] = data?.pages.flatMap((p) => p.items) ?? []

  return (
    <div className="flex h-full flex-col">
      {/* Page filter strip */}
      <div className="flex shrink-0 items-center gap-2 border-b border-border px-4 py-2">
        <Filter className="size-3.5 text-muted-foreground" />
        <span className="text-xs text-muted-foreground">Filters:</span>
        <FilterDropdown
          label="Agent kind"
          options={AGENT_KIND_OPTIONS}
          selected={agentKindFilter}
          onChange={setAgentKindFilter}
        />
      </div>

      {/* Rows */}
      <div className="flex-1 overflow-auto">
        {isLoading && items.length === 0 ? (
          <div className="flex h-60 items-center justify-center">
            <Loader2 className="size-5 animate-spin text-muted-foreground" />
          </div>
        ) : isError ? (
          <div className="flex h-60 items-center justify-center text-sm text-destructive">
            Failed to load sessions: {error?.message}
          </div>
        ) : items.length === 0 ? (
          <div className="flex h-60 items-center justify-center text-sm text-muted-foreground">
            No sessions found in the selected time range
          </div>
        ) : (
          items.map((item) => (
            <SessionRow key={`${item.source_id}/${item.session_id}`} item={item} />
          ))
        )}
      </div>

      {/* Load more */}
      {hasNextPage && (
        <div className="shrink-0 border-t border-border py-3 text-center">
          <button
            onClick={() => fetchNextPage()}
            disabled={isFetchingNextPage}
            className={cn(
              "rounded border border-border bg-background px-4 py-1.5 text-sm text-muted-foreground transition-colors",
              !isFetchingNextPage && "hover:bg-muted hover:text-foreground",
            )}
          >
            {isFetchingNextPage ? (
              <span className="inline-flex items-center gap-2">
                <Loader2 className="size-3.5 animate-spin" /> Loading…
              </span>
            ) : (
              "Load more"
            )}
          </button>
        </div>
      )}
    </div>
  )
}
