/**
 * Helpers for reading the pair-sweeper's `metadata.proxy` block off a
 * turn detail / turn list item. The metadata column is open-ended JSON
 * (typed `unknown` in the API), so callers walk it defensively.
 *
 * Lives outside any single component because both the agent-turn detail
 * panel and the GanttNav surface multi-leg indicators.
 */

export type ProxyRole =
  | "proxy_in"
  | "proxy_out"
  | "mirror_primary"
  | "mirror_secondary"

export interface ProxyMeta {
  role: ProxyRole | string
  pair_id?: string
  peer_turn_id?: string
  peer_turn_ids?: string[]
}

/** Pull the `metadata.proxy` block out of an arbitrary JSON-ish value.
 * Returns `null` when no metadata.proxy block is present (direct turn). */
export function readProxyMeta(metadata: unknown): ProxyMeta | null {
  if (!metadata || typeof metadata !== "object") return null
  const proxy = (metadata as Record<string, unknown>).proxy
  if (!proxy || typeof proxy !== "object") return null
  const role = (proxy as Record<string, unknown>).role
  if (typeof role !== "string") return null
  return proxy as unknown as ProxyMeta
}

/** Number of legs in a turn's proxy group, including the turn itself.
 * Returns 0 when the turn is not part of any group. */
export function proxyGroupSize(proxy: ProxyMeta | null): number {
  if (!proxy) return 0
  if (proxy.peer_turn_ids && proxy.peer_turn_ids.length > 0) {
    return proxy.peer_turn_ids.length + 1
  }
  if (proxy.peer_turn_id) return 2
  return 1
}
