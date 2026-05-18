import { ArrowLeftRight, Copy } from "lucide-react"
import { cn } from "@/lib/utils"
import type { AgentTurnListItem } from "@/types/api"

/**
 * Inline indicator on agent-turn rows that the backend pair sweeper
 * matched this row with another captured-but-hidden leg.
 *
 * * `proxy_in` — outer leg of a real proxy hop (haproxy / litellm). The
 *   inner upstream leg is folded out of the default list view.
 * * `mirror_primary` — same packet captured on two interfaces (br0 +
 *   docker0 typically). The other copy is folded out.
 *
 * Hidden roles (`proxy_out` / `mirror_secondary`) never reach this
 * component when the default `include_proxy_hops=false` is in effect;
 * we render the badge for them too (with a `(hop)` label) so users who
 * toggle hops on can see which side they're looking at.
 */
export function ProxyBadge({ item }: { item: AgentTurnListItem }) {
  if (!item.proxy_role) return null
  const role = item.proxy_role
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
  const peer = item.proxy_peer_turn_id
  const title = peer
    ? `${label} — peer turn ${peer}`
    : label
  return (
    <span
      title={title}
      className={cn(
        "inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-medium",
        // Visible-by-default legs get a subtle blue; hidden hops get a
        // gray "secondary" treatment.
        isPrimary
          ? "bg-blue-500/10 text-blue-600 dark:text-blue-300"
          : "bg-muted text-muted-foreground",
      )}
    >
      <Icon className="size-3" />
      {label}
    </span>
  )
}
