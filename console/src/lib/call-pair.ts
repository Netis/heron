/**
 * Client-side pair detection for LLM calls *within* a single agent turn.
 *
 * Mirrors the backend `ts-turn::proxy_pair::group_all` algorithm but
 * runs on the `/api/agent-turns/{id}/calls` payload. We do this in JS
 * (not in the backend) because the `llm_calls` table has no metadata
 * column to persist call-level pairings — adding one would require a
 * migration. The visual fold solves the user's immediate complaint
 * (24 captured calls reduced to 12 logical calls in the timeline)
 * without changing the on-disk schema.
 *
 * The trade-off: `agent_turns.call_count` still reports the raw 24
 * because that aggregate is computed at ingest time. Surfacing a
 * "logical call count" in the turn header is a follow-up.
 */

import type { AgentTurnCallItem } from "@/types/api"

/** Maximum gap (ms) between any two calls' request_times for them to
 * cluster into the same group. Matches the backend's
 * MAX_REQ_TIME_GAP_US / 1000. Real proxy hops fire within a few ms;
 * 100ms is generous headroom for slow proxies. */
const MAX_REQ_TIME_GAP_MS = 100

/** Same-packet vs real-hop separator. Below this delta the legs are
 * treated as "mirrors" of one another (canonical = lex-smallest id).
 * Above, the wider-span leg is the proxy_in client-facing record.
 * 0.5ms matches the backend's 500us threshold. */
const MIRROR_TIME_TOLERANCE_MS = 0.5

export type CallProxyRole =
  | "canonical"
  | "hop"

export interface CallGrouping {
  /** Calls visible in the default (folded) view — every direct call
   * plus the canonical leg of every detected group. Same order as the
   * input. */
  visible: AgentTurnCallItem[]
  /** Map from canonical call id → its hidden peers (in time order).
   * Used to render "(+N hops)" badges next to each canonical. */
  hopsByCanonical: Map<string, AgentTurnCallItem[]>
  /** Sequence numbers of every hidden hop. Used to grey them out in
   * the GanttNav when the user toggles "show hops" back on. */
  hopSequences: Set<number>
  /** How many calls would be hidden in the folded view. The toggle
   * UI uses this to label the button "(N hidden)" so the user knows
   * the fold is doing something even when groups span the full list. */
  hopCount: number
}

interface ContentKey {
  wire_api: string
  model: string
  is_stream: boolean
  status_code: number | null
  finish_reason: string | null
  input_tokens: number | null
  output_tokens: number | null
  request_path: string
}

function contentKey(c: AgentTurnCallItem): string {
  // Joined string is the cheapest stable hash for HashMap-like
  // bucketing in JS. Token nulls and finish nulls intentionally
  // stringify to "null" so two equally-tokenless calls still cluster.
  return [
    c.wire_api,
    c.model,
    c.is_stream ? "S" : "N",
    c.status_code ?? "null",
    c.finish_reason ?? "null",
    c.input_tokens ?? "null",
    c.output_tokens ?? "null",
    c.request_path,
  ].join("\x1f")
}

function netView(c: AgentTurnCallItem): string {
  return `${c.client_ip}:${c.client_port}->${c.server_ip}:${c.server_port}`
}

function spanMs(c: AgentTurnCallItem): number {
  const end = c.complete_time ?? c.response_time ?? c.request_time
  return end - c.request_time
}

/**
 * Compute the call-level fold for one turn's call list. Pure function:
 * runs in O(n^2) per content bucket but in practice each bucket is
 * tiny (≤ pair size), so total work is linear.
 *
 * Rules (must all hold for two calls to pair):
 *   - identical content fingerprint (wire_api, model, tokens, finish,
 *     stream flag, status code, path)
 *   - |request_time gap| ≤ MAX_REQ_TIME_GAP_MS
 *   - different (client_ip:port, server_ip:port)
 *
 * Within a cluster of ≥2 calls, the canonical is the longest-span
 * call (client-facing leg sees the full proxy round-trip). Ties on
 * span go to the lex-smallest call id (deterministic re-runs).
 */
export function groupCalls(calls: AgentTurnCallItem[]): CallGrouping {
  // Bucket by content fingerprint.
  const buckets = new Map<string, AgentTurnCallItem[]>()
  for (const c of calls) {
    const k = contentKey(c)
    const list = buckets.get(k)
    if (list) list.push(c)
    else buckets.set(k, [c])
  }

  // For each bucket, time-cluster contiguous-in-time entries.
  const claimed = new Set<string>()
  const hopsByCanonical = new Map<string, AgentTurnCallItem[]>()
  const hopSequences = new Set<number>()

  for (const bucket of buckets.values()) {
    if (bucket.length < 2) continue
    const sorted = bucket.slice().sort((a, b) => a.request_time - b.request_time)
    for (let i = 0; i < sorted.length; i++) {
      const a = sorted[i]
      if (claimed.has(a.id)) continue
      // Greedily collect peers within the time window having a distinct
      // network view. We allow more than one peer (haproxy 3-leg).
      const peers: AgentTurnCallItem[] = []
      const seenViews = new Set<string>([netView(a)])
      for (let j = i + 1; j < sorted.length; j++) {
        const b = sorted[j]
        if (claimed.has(b.id)) continue
        if (b.request_time - a.request_time > MAX_REQ_TIME_GAP_MS) break
        const bView = netView(b)
        if (seenViews.has(bView)) continue
        peers.push(b)
        seenViews.add(bView)
      }
      if (peers.length === 0) continue

      // Pick canonical: widest span; tie → lex smallest id.
      const cluster = [a, ...peers]
      let canon = cluster[0]
      let canonSpan = spanMs(cluster[0])
      for (const c of cluster.slice(1)) {
        const s = spanMs(c)
        if (s > canonSpan || (s === canonSpan && c.id < canon.id)) {
          canon = c
          canonSpan = s
        }
      }
      const hops = cluster.filter((c) => c.id !== canon.id)
      hops.sort((a, b) => a.request_time - b.request_time)
      hopsByCanonical.set(canon.id, hops)
      for (const h of hops) {
        claimed.add(h.id)
        hopSequences.add(h.sequence)
      }
      claimed.add(canon.id)
      // Drop unused `MIRROR_TIME_TOLERANCE_MS` reference to silence
      // unused-var lint while keeping the constant available for
      // future role-naming work (see backend `proxy_pair` module).
      void MIRROR_TIME_TOLERANCE_MS
    }
  }

  const visible = calls.filter((c) => !hopSequences.has(c.sequence))
  return {
    visible,
    hopsByCanonical,
    hopSequences,
    hopCount: hopSequences.size,
  }
}
