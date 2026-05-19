/**
 * Proxy View — surfaces what a llmproxy mutated across the captured legs
 * of one logical LLM call. Shown only when the current turn is part of a
 * proxy group (i.e. `turn.proxy_role` is set).
 *
 * Layout, top to bottom:
 *   1. Group topology — the IPs/ports + role of every leg, in canonical
 *      order, with latency next to each.
 *   2. Latency breakdown — client_observed / upstream_observed / proxy
 *      overhead derived from the role pairing.
 *   3. Optional Model Rewrite banner when the proxy forwarded under a
 *      different model name than the client requested.
 *   4. Response header diff — usually the most interesting (LiteLLM
 *      injects x-litellm-* on the response back to the client, while
 *      upstream returns provider IDs like anthropic-request-id only on
 *      its leg). Common headers collapse to a single row; modified
 *      headers spread values per leg; per-leg headers show which role
 *      injected/owns them.
 *   5. Request header diff — secondary; usually `Host` rewrites are
 *      what live here.
 *
 * The component is intentionally read-only: it doesn't expose links to
 * jump into hidden legs (the user already chose the canonical leg by
 * opening this turn). Drilling into a peer is a separate flow — copy
 * the turn_id out of the badge and navigate manually.
 */
import { useMemo } from "react"
import { Loader2, ArrowRightLeft, ArrowDownToLine, ArrowUpFromLine } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs } from "@/lib/format"
import { useAgentTurnProxyView } from "@/hooks/use-agent-turns"
import type { HeaderDiffEntry, ProxyViewMember } from "@/types/api"

interface Props {
  turnId: string
}

export function ProxyViewTab({ turnId }: Props) {
  const { data, isLoading, isError, error } = useAgentTurnProxyView(turnId)

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-12 text-muted-foreground">
        <Loader2 className="size-4 animate-spin" />
      </div>
    )
  }
  if (isError || !data) {
    return (
      <div className="rounded border border-border bg-muted/30 px-3 py-4 text-xs text-muted-foreground">
        Proxy view unavailable: {error?.message ?? "no group data"}
      </div>
    )
  }

  return (
    <div className="flex flex-col gap-4 p-3 text-sm">
      <TopologySection members={data.members} />
      <LatencySection breakdown={data.latency_breakdown} />
      {data.model_rewrite && (
        <ModelRewriteBanner rewrite={data.model_rewrite} />
      )}
      <HeaderDiffSection
        title="Response headers"
        entries={data.response_header_diff}
        members={data.members}
        defaultExpanded
      />
      <HeaderDiffSection
        title="Request headers"
        entries={data.request_header_diff}
        members={data.members}
      />
    </div>
  )
}

// ----- Topology -----

function TopologySection({ members }: { members: ProxyViewMember[] }) {
  return (
    <section>
      <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
        Topology
      </h3>
      <div className="flex flex-col gap-2">
        {members.map((m, i) => (
          <div
            key={m.turn_id}
            className="flex items-center gap-3 rounded border border-border bg-muted/30 px-3 py-2 text-xs"
          >
            <RoleChip role={m.role} />
            <span className="font-mono">
              {m.client_ip}{m.client_port ? `:${m.client_port}` : ""}
              {" → "}
              {m.server_ip}{m.server_port ? `:${m.server_port}` : ""}
            </span>
            <span className="ml-auto tabular-nums text-muted-foreground">
              {formatMs(m.e2e_latency_ms)}
            </span>
            <span
              className="cursor-help font-mono text-[10px] text-muted-foreground/70"
              title={m.turn_id}
            >
              {m.turn_id.slice(-12)}
            </span>
            {i < members.length - 1 && (
              <ArrowRightLeft className="ml-1 size-3 text-muted-foreground/60" />
            )}
          </div>
        ))}
      </div>
    </section>
  )
}

const ROLE_LABEL: Record<string, { label: string; tone: string }> = {
  proxy_in: { label: "Client-facing", tone: "bg-blue-500/15 text-blue-700 dark:text-blue-300" },
  proxy_out: { label: "Upstream hop", tone: "bg-amber-500/15 text-amber-700 dark:text-amber-300" },
  mirror_primary: { label: "Captured (br0)", tone: "bg-emerald-500/15 text-emerald-700 dark:text-emerald-300" },
  mirror_secondary: { label: "Captured (docker0)", tone: "bg-muted text-muted-foreground" },
}

function RoleChip({ role }: { role: string }) {
  const info = ROLE_LABEL[role] ?? { label: role, tone: "bg-muted text-muted-foreground" }
  return (
    <span
      className={cn("rounded px-1.5 py-0.5 text-[10px] font-medium", info.tone)}
      title={role}
    >
      {info.label}
    </span>
  )
}

// ----- Latency breakdown -----

function LatencySection({ breakdown }: { breakdown: { client_observed_ms: number | null; upstream_observed_ms: number | null; proxy_overhead_ms: number | null } }) {
  if (
    breakdown.client_observed_ms == null
    && breakdown.upstream_observed_ms == null
    && breakdown.proxy_overhead_ms == null
  ) {
    return null
  }
  return (
    <section>
      <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
        Latency
      </h3>
      <div className="grid grid-cols-3 gap-3">
        <Stat label="Client observed" value={formatMs(breakdown.client_observed_ms)} />
        <Stat label="Upstream observed" value={formatMs(breakdown.upstream_observed_ms)} />
        <Stat
          label="Proxy overhead"
          value={formatMs(breakdown.proxy_overhead_ms)}
          tone={breakdown.proxy_overhead_ms != null && breakdown.proxy_overhead_ms > 100 ? "warn" : undefined}
        />
      </div>
    </section>
  )
}

function Stat({ label, value, tone }: { label: string; value: string; tone?: "warn" }) {
  return (
    <div className="rounded border border-border bg-muted/30 px-3 py-2">
      <div className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div className={cn("mt-0.5 text-base font-medium tabular-nums", tone === "warn" && "text-amber-600 dark:text-amber-300")}>
        {value}
      </div>
    </div>
  )
}

// ----- Model rewrite -----

function ModelRewriteBanner({ rewrite }: { rewrite: { client_requested: string | null; upstream_received: string | null } }) {
  return (
    <section className="rounded border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs">
      <div className="flex items-center gap-2 font-semibold">
        <ArrowRightLeft className="size-3.5" />
        Model rewrite
      </div>
      <div className="mt-1 flex items-center gap-2 font-mono">
        <code>{rewrite.client_requested ?? "—"}</code>
        <span className="text-muted-foreground">→</span>
        <code>{rewrite.upstream_received ?? "—"}</code>
      </div>
    </section>
  )
}

// ----- Header diff -----

function HeaderDiffSection({
  title,
  entries,
  members,
  defaultExpanded = false,
}: {
  title: string
  entries: HeaderDiffEntry[]
  members: ProxyViewMember[]
  defaultExpanded?: boolean
}) {
  const grouped = useMemo(() => {
    const common = entries.filter((e) => e.kind === "common")
    const modified = entries.filter((e) => e.kind === "modified")
    const perLeg = entries.filter((e) => e.kind === "per_leg")
    return { common, modified, perLeg }
  }, [entries])

  return (
    <section>
      <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
        {title}
        <span className="ml-2 font-mono text-[10px] text-muted-foreground/70">
          {grouped.modified.length} modified · {grouped.perLeg.length} per-leg · {grouped.common.length} common
        </span>
      </h3>
      {grouped.modified.length > 0 && (
        <HeaderDiffTable entries={grouped.modified} members={members} kindBadge="modified" />
      )}
      {grouped.perLeg.length > 0 && (
        <HeaderDiffTable entries={grouped.perLeg} members={members} kindBadge="per_leg" />
      )}
      {grouped.common.length > 0 && (
        <details className="mt-1 rounded border border-border bg-muted/20" open={defaultExpanded ? false : false}>
          <summary className="cursor-pointer select-none px-3 py-1.5 text-xs text-muted-foreground">
            {grouped.common.length} common header(s)
          </summary>
          <HeaderDiffTable entries={grouped.common} members={members} kindBadge="common" />
        </details>
      )}
    </section>
  )
}

function HeaderDiffTable({
  entries,
  members,
  kindBadge,
}: {
  entries: HeaderDiffEntry[]
  members: ProxyViewMember[]
  kindBadge: "common" | "modified" | "per_leg"
}) {
  return (
    <div className="overflow-hidden rounded border border-border">
      <table className="w-full text-xs">
        <tbody>
          {entries.map((e) => (
            <HeaderRow key={e.name} entry={e} members={members} kindBadge={kindBadge} />
          ))}
        </tbody>
      </table>
    </div>
  )
}

function HeaderRow({
  entry,
  members,
  kindBadge,
}: {
  entry: HeaderDiffEntry
  members: ProxyViewMember[]
  kindBadge: "common" | "modified" | "per_leg"
}) {
  // For "common", just one row with the single shared value.
  if (kindBadge === "common") {
    const value = entry.values[0]?.value ?? ""
    return (
      <tr className="border-b border-border/50 last:border-b-0">
        <td className="w-1/3 truncate px-3 py-1.5 font-mono text-muted-foreground" title={entry.name}>
          {entry.name}
        </td>
        <td className="px-3 py-1.5 font-mono text-muted-foreground/70" title={value}>
          {truncate(value, 120)}
        </td>
      </tr>
    )
  }

  // For modified / per_leg, expand one sub-row per value to clarify
  // which leg owns which value.
  const totalLegs = members.length
  const presentRoles = new Set(entry.values.map((v) => v.role))
  const missingMembers = members.filter((m) => !presentRoles.has(m.role) && totalLegs !== entry.values.length)
  return (
    <>
      <tr className="border-b border-border/50">
        <td
          className="w-1/3 truncate px-3 py-1.5 font-mono"
          title={entry.name}
          rowSpan={entry.values.length + (missingMembers.length > 0 ? 1 : 0)}
        >
          <div className="flex items-center gap-1.5">
            {entry.name}
            <KindBadge kind={kindBadge} />
          </div>
        </td>
        <td className="px-3 py-1.5">
          <ValueCell value={entry.values[0]} />
        </td>
      </tr>
      {entry.values.slice(1).map((v, i) => (
        <tr key={i} className="border-b border-border/50">
          <td className="px-3 py-1.5">
            <ValueCell value={v} />
          </td>
        </tr>
      ))}
      {missingMembers.length > 0 && (
        <tr className="border-b border-border/50 last:border-b-0">
          <td className="px-3 py-1.5 text-muted-foreground/60">
            <span className="font-mono text-[10px]">
              {missingMembers.map((m) => roleShort(m.role)).join(", ")}: <span className="italic">absent</span>
            </span>
          </td>
        </tr>
      )}
    </>
  )
}

function ValueCell({ value }: { value: { role: string; value: string; turn_id: string } }) {
  return (
    <div className="flex items-baseline gap-2">
      <span className="shrink-0 font-mono text-[10px] text-muted-foreground" title={value.turn_id}>
        {roleShort(value.role)}
      </span>
      <span className="break-all font-mono" title={value.value}>
        {truncate(value.value, 180)}
      </span>
    </div>
  )
}

function KindBadge({ kind }: { kind: "common" | "modified" | "per_leg" }) {
  if (kind === "common") return null
  if (kind === "modified") {
    return (
      <span className="inline-flex items-center gap-0.5 rounded bg-amber-500/15 px-1 py-0.5 text-[9px] font-medium text-amber-700 dark:text-amber-300">
        <ArrowRightLeft className="size-2.5" />
        modified
      </span>
    )
  }
  return (
    <span className="inline-flex items-center gap-0.5 rounded bg-blue-500/15 px-1 py-0.5 text-[9px] font-medium text-blue-700 dark:text-blue-300">
      <ArrowDownToLine className="size-2.5" />
      per leg
    </span>
  )
}

function roleShort(role: string): string {
  switch (role) {
    case "proxy_in":
      return "in"
    case "proxy_out":
      return "out"
    case "mirror_primary":
      return "mir1"
    case "mirror_secondary":
      return "mir2"
    default:
      return role
  }
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s
  return s.slice(0, max) + "…"
}

// Re-exported but not used directly — keeps the lucide import lean while
// allowing the component to evolve without dropping it.
export { ArrowUpFromLine }
