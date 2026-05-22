/**
 * In-turn Proxy View — used when call-level duplicates are folded
 * client-side but the *turn* itself is not part of a backend proxy
 * group. Common scenario: LiteLLM is captured on the same host and
 * every LLM call is recorded twice (once on the client-facing port,
 * once on the upstream-facing port) under one logical agent turn.
 *
 * Renders one card per folded pair group:
 *   - the canonical leg's 5-tuple + status + latency
 *   - each hidden hop's 5-tuple + status + latency
 *   - proxy_overhead_ms = canonical.e2e - hop.e2e (when both known)
 *   - model rewrite line when the canonical and hop models differ
 *
 * The header-diff view (response x-litellm-* etc.) would require
 * parsing the headers blob client-side and is intentionally left as a
 * follow-up — the v1 surfaces topology + timing + model, which is what
 * most users want first.
 */
import { ArrowRightLeft, Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs } from "@/lib/format"
import type { AgentTurnCallItem } from "@/types/api"

interface Props {
  hopsByCanonical: Map<string, AgentTurnCallItem[]>
  /** All visible (canonical) calls in their original order — we look
   * up each canonical's hops in the map. */
  canonicals: AgentTurnCallItem[]
}

export function InTurnProxyView({ hopsByCanonical, canonicals }: Props) {
  // Only canonical calls that actually have folded hops show up here;
  // direct calls without proxy duplicates are not part of any pair
  // group and don't need a card.
  const groups = canonicals.filter((c) => (hopsByCanonical.get(c.id) ?? []).length > 0)
  if (groups.length === 0) {
    return (
      <div className="px-3 py-4 text-xs text-muted-foreground">
        No call-level proxy duplicates detected in this turn.
      </div>
    )
  }
  return (
    <div className="flex flex-col gap-3 p-3 text-sm">
      <div className="rounded border border-border bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
        This turn isn't part of a backend proxy group, but{" "}
        <span className="font-semibold">{groups.length} of its LLM calls</span>{" "}
        were captured at two vantage points (client → proxy and proxy →
        upstream). The cards below pair them up with the matching peer.
      </div>
      {groups.map((c) => (
        <CallPairCard key={c.id} canonical={c} hops={hopsByCanonical.get(c.id) ?? []} />
      ))}
    </div>
  )
}

function CallPairCard({
  canonical,
  hops,
}: {
  canonical: AgentTurnCallItem
  hops: AgentTurnCallItem[]
}) {
  return (
    <section className="rounded border border-border">
      <header className="flex items-center gap-2 border-b border-border bg-muted/40 px-3 py-2 text-xs">
        <Layers className="size-3 text-blue-500" />
        <span className="font-medium">Call #{canonical.sequence}</span>
        <span className="text-muted-foreground">
          + {hops.length} folded hop{hops.length > 1 ? "s" : ""}
        </span>
      </header>
      <div className="flex flex-col gap-1.5 px-3 py-2">
        <CallRow call={canonical} role="canonical" />
        {hops.map((h) => (
          <CallRow
            key={h.id}
            call={h}
            role="hop"
            overheadVs={canonical}
            modelOf={canonical}
          />
        ))}
      </div>
    </section>
  )
}

function CallRow({
  call,
  role,
  overheadVs,
  modelOf,
}: {
  call: AgentTurnCallItem
  role: "canonical" | "hop"
  overheadVs?: AgentTurnCallItem
  modelOf?: AgentTurnCallItem
}) {
  const overhead =
    overheadVs?.e2e_latency_ms != null && call.e2e_latency_ms != null
      ? overheadVs.e2e_latency_ms - call.e2e_latency_ms
      : null
  const modelRewrite = modelOf && modelOf.model !== call.model
  return (
    <div className="flex items-center gap-3 text-xs">
      <span
        className={cn(
          "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium",
          role === "canonical"
            ? "bg-blue-500/15 text-blue-700 dark:text-blue-300"
            : "bg-muted text-muted-foreground",
        )}
      >
        {role === "canonical" ? "Client-facing" : "Proxy hop"}
      </span>
      <span className="font-mono">
        {call.client_ip}:{call.client_port} → {call.server_ip}:{call.server_port}
      </span>
      {modelRewrite && (
        <span
          className="inline-flex items-center gap-1 rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] text-amber-700 dark:text-amber-300"
          title={`Model rewrite: ${modelOf?.model} → ${call.model}`}
        >
          <ArrowRightLeft className="size-2.5" />
          {call.model}
        </span>
      )}
      <span className="ml-auto tabular-nums text-muted-foreground">
        {formatMs(call.e2e_latency_ms)}
      </span>
      {overhead != null && (
        <span
          className={cn(
            "tabular-nums font-mono text-[10px]",
            overhead > 5 ? "text-amber-600 dark:text-amber-300" : "text-muted-foreground",
          )}
          title="Proxy overhead — canonical e2e − hop e2e"
        >
          Δ{overhead >= 0 ? "+" : ""}
          {overhead.toFixed(1)}ms
        </span>
      )}
    </div>
  )
}
