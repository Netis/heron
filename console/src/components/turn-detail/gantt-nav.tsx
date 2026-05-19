import { useMemo } from "react"
import { ArrowLeftRight, Copy, Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatDuration, formatMs } from "@/lib/format"
import { classifyType } from "@/lib/wire-apis/dispatch"
import { GanttCallTypeIcon } from "@/components/call-renderers/chips/dispatch"
import { finishTone } from "@/lib/finish-tone"
import { readProxyMeta, proxyGroupSize } from "@/lib/proxy-meta"
import type { AgentTurnCallItem, AgentTurnDetail } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
  activeSequence: number | null
  onSelect: (sequence: number) => void
  /** When the parent panel folds call-level proxy duplicates, this map
   * tells GanttNav which canonical call ids carry hidden hops so it
   * can stack a small "+N" indicator on those bars. Empty map (default)
   * keeps the timeline a flat per-call view. */
  hopsByCanonical?: Map<string, AgentTurnCallItem[]>
}

const SLOW_THRESHOLD_MS = 10_000

function classifySpeed(call: AgentTurnCallItem): "normal" | "slow" | "warn" | "error" {
  if ((call.status_code ?? 0) >= 400) return "error"
  const tone = finishTone(call.finish_reason)
  if (tone === "err") return "error"
  if (tone === "warn") return "warn"
  if ((call.e2e_latency_ms ?? 0) > SLOW_THRESHOLD_MS) return "slow"
  return "normal"
}


/**
 * Compact badge surfaced under the Timeline header when the turn is one
 * leg of a proxy group. Tells the user "this turn is folded together
 * with N other captured legs — see the Proxy view tab for the merged
 * view". Color follows the role-tone palette used by `ProxyBadge` in
 * the agent-turns list so the two callsites are visually consistent.
 */
function MultiLegBadge({
  proxy,
  groupSize,
}: {
  proxy: ReturnType<typeof readProxyMeta>
  groupSize: number
}) {
  if (!proxy) return null
  const role = proxy.role
  const isPrimary = role === "proxy_in" || role === "mirror_primary"
  const Icon = isPrimary ? ArrowLeftRight : Copy
  const label =
    role === "proxy_in"
      ? "via proxy"
      : role === "proxy_out"
        ? "proxy hop"
        : role === "mirror_primary"
          ? "mirrored"
          : "mirror copy"
  const peers = proxy.peer_turn_ids ?? (proxy.peer_turn_id ? [proxy.peer_turn_id] : [])
  const title = peers.length > 0
    ? `${label} — group of ${groupSize} captured legs:\n${peers.join("\n")}`
    : label
  return (
    <div
      title={title}
      className={cn(
        "mt-1 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-medium",
        isPrimary
          ? "bg-blue-500/10 text-blue-600 dark:text-blue-300"
          : "bg-muted text-muted-foreground",
      )}
    >
      <Icon className="size-3" />
      <span>{groupSize}-leg {label}</span>
    </div>
  )
}

export function GanttNav({ turn, calls, activeSequence, onSelect, hopsByCanonical }: Props) {
  const { minStart, total } = useMemo(() => {
    if (calls.length === 0) return { minStart: turn.start_time, total: turn.duration_ms || 1 }
    const min = Math.min(...calls.map((c) => c.request_time))
    const max = Math.max(...calls.map((c) => c.complete_time ?? c.response_time ?? c.request_time))
    return { minStart: min, total: Math.max(max - min, 1) }
  }, [calls, turn])

  const types = useMemo(
    () => calls.map((c) => classifyType(c.wire_api, c.response_body, c.id, turn.final_call_id)),
    [calls, turn.final_call_id],
  )

  const proxy = readProxyMeta(turn.metadata)
  const groupSize = proxyGroupSize(proxy)

  return (
    <aside className="flex w-[140px] shrink-0 flex-col border-r border-border">
      <div className="shrink-0 border-b border-border px-3 py-2">
        <div className="text-xs font-medium">Timeline</div>
        <div className="text-[11px] tabular-nums text-muted-foreground">{formatDuration(turn.duration_ms)}</div>
        {proxy && groupSize >= 2 && <MultiLegBadge proxy={proxy} groupSize={groupSize} />}
      </div>
      <div className="flex-1 overflow-y-auto p-1">
        {calls.length === 0 ? (
          <div className="flex h-20 items-center justify-center text-xs text-muted-foreground">No calls</div>
        ) : (
          calls.map((c, i) => {
            const end = c.complete_time ?? c.response_time ?? c.request_time
            const offset = ((c.request_time - minStart) / total) * 100
            const width = Math.max(((end - c.request_time) / total) * 100, 0.5)
            const speed = classifySpeed(c)
            const hops = hopsByCanonical?.get(c.id) ?? []
            return (
              <button
                key={c.id}
                onClick={() => onSelect(c.sequence)}
                title={hops.length > 0
                  ? `Folded ${hops.length} proxy-duplicate leg(s) under this call`
                  : undefined}
                className={cn(
                  "grid w-full grid-cols-[16px_16px_1fr_36px] items-center gap-1 rounded px-1 py-1 text-left text-[10px]",
                  activeSequence === c.sequence ? "bg-blue-50 dark:bg-blue-950/40" : "hover:bg-muted/60",
                  (speed === "slow" || speed === "warn") && "border-l-2 border-amber-500/70",
                  speed === "error" && "border-l-2 border-red-500/70",
                  hops.length > 0 && speed === "normal" && "border-l-2 border-blue-500/70",
                )}
              >
                <span className="tabular-nums text-muted-foreground">{c.sequence}</span>
                <GanttCallTypeIcon callType={types[i]} />
                <div className="relative h-2 rounded bg-muted">
                  <div
                    className={cn(
                      "absolute top-0 h-full rounded",
                      (speed === "slow" || speed === "warn") && "bg-amber-500/80",
                      speed === "error" && "bg-red-500/80",
                      speed === "normal" && "bg-blue-400",
                    )}
                    style={{ left: `${offset}%`, width: `${width}%`, minWidth: "2px" }}
                  />
                  {/* Folded-hop overlay: a thin underline-style bar
                      directly below the main bar, indicating one or
                      more captured peers were folded into this leg.
                      Width matches the main bar so the eye reads it
                      as a "shadow" of the same call. */}
                  {hops.length > 0 && (
                    <div
                      className="absolute -bottom-1 h-0.5 rounded bg-blue-500/60"
                      style={{ left: `${offset}%`, width: `${width}%`, minWidth: "2px" }}
                    />
                  )}
                </div>
                <span className={cn(
                  "text-right tabular-nums",
                  (speed === "slow" || speed === "warn") && "text-amber-600",
                  speed === "error" && "text-red-600",
                  speed === "normal" && "text-muted-foreground",
                )}>
                  {hops.length > 0 && (
                    <span className="mr-1 inline-flex items-center text-blue-500" title={`+${hops.length} folded hop(s)`}>
                      <Layers className="size-2.5" />
                    </span>
                  )}
                  {formatMs(c.e2e_latency_ms)}
                </span>
              </button>
            )
          })
        )}
      </div>
    </aside>
  )
}
