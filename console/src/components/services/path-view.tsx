import { useMemo } from "react"
import { Users } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatNumber } from "@/lib/format"
import type { ServicesTopology, TopologyNode } from "@/types/api"

const APP_NODE_STYLE: Record<string, string> = {
  vllm: "border-purple-400 bg-purple-50 dark:bg-purple-950/40",
  sglang: "border-cyan-400 bg-cyan-50 dark:bg-cyan-950/40",
  ollama: "border-amber-400 bg-amber-50 dark:bg-amber-950/40",
  llamacpp: "border-emerald-400 bg-emerald-50 dark:bg-emerald-950/40",
  litellm: "border-pink-400 bg-pink-50 dark:bg-pink-950/40",
  openai: "border-green-400 bg-green-50 dark:bg-green-950/40",
  anthropic: "border-orange-400 bg-orange-50 dark:bg-orange-950/40",
  gemini: "border-blue-400 bg-blue-50 dark:bg-blue-950/40",
  clients: "border-slate-400 bg-slate-100 dark:bg-slate-800/60",
}

const APP_BADGE_STYLE: Record<string, string> = {
  vllm: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  sglang: "bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300",
  ollama: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
  llamacpp: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
  litellm: "bg-pink-100 text-pink-800 dark:bg-pink-900/40 dark:text-pink-300",
  openai: "bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300",
  anthropic: "bg-orange-100 text-orange-800 dark:bg-orange-900/40 dark:text-orange-300",
  gemini: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
}

const CLIENTS_ID = "__clients__:0"
const NODE_W = 200
const NODE_H = 80
const COL_GAP = 120
const ROW_GAP = 24
const SVG_PAD = 24

function nodeId(ip: string, port: number): string {
  return `${ip}:${port}`
}

interface LayoutNode extends TopologyNode {
  id: string
  col: number
  row: number
  x: number
  y: number
}

interface LayoutEdge {
  from: string
  to: string
  kind: "proxy" | "client"
  turn_count: number
}

/// BFS from the clients node assigns each service to a depth column.
/// Services not reachable from clients (e.g. an isolated proxy_out
/// without observed proxy_in this window) get appended to the last
/// column so they still render somewhere. Sibling order within a
/// column is stable by `call_count` desc for visual predictability.
function layoutGraph(topology: ServicesTopology) {
  const nodesById = new Map<string, TopologyNode>()
  for (const n of topology.nodes) {
    nodesById.set(nodeId(n.server_ip, n.server_port), n)
  }
  const adj = new Map<string, string[]>()
  for (const e of topology.edges) {
    const from = nodeId(e.from_ip, e.from_port)
    const to = nodeId(e.to_ip, e.to_port)
    if (!adj.has(from)) adj.set(from, [])
    adj.get(from)!.push(to)
  }

  const colOf = new Map<string, number>()
  const queue: Array<{ id: string; col: number }> = []
  if (nodesById.has(CLIENTS_ID)) {
    queue.push({ id: CLIENTS_ID, col: 0 })
    colOf.set(CLIENTS_ID, 0)
  }
  while (queue.length > 0) {
    const { id, col } = queue.shift()!
    const neighbors = adj.get(id) ?? []
    for (const nb of neighbors) {
      if (!nodesById.has(nb)) continue
      const existing = colOf.get(nb)
      // Keep the LARGEST depth so a node always sits to the right of
      // any predecessor we've actually seen — matters for the
      // 3-leg haproxy case where clients → A → B AND clients → B
      // both exist and B should still render to the right of A.
      if (existing === undefined || col + 1 > existing) {
        colOf.set(nb, col + 1)
        queue.push({ id: nb, col: col + 1 })
      }
    }
  }

  // Place stragglers (no incoming edge from clients) into the
  // rightmost discovered column + 1 so they render but don't overlap.
  let maxCol = 0
  for (const c of colOf.values()) maxCol = Math.max(maxCol, c)
  for (const id of nodesById.keys()) {
    if (!colOf.has(id)) colOf.set(id, maxCol + 1)
  }

  // Group by column, sort within by call_count desc.
  const cols: LayoutNode[][] = []
  for (const [id, n] of nodesById) {
    const c = colOf.get(id) ?? 0
    if (!cols[c]) cols[c] = []
    cols[c].push({
      ...n,
      id,
      col: c,
      row: 0, // assigned below
      x: 0,
      y: 0,
    })
  }
  for (const colNodes of cols) {
    if (!colNodes) continue
    colNodes.sort((a, b) => b.call_count - a.call_count)
    colNodes.forEach((n, i) => {
      n.row = i
    })
  }

  const colCount = cols.length
  // Center each column vertically so columns of unequal length don't
  // visually drift to the top — easier to scan upstream/downstream.
  const maxRows = Math.max(1, ...cols.filter(Boolean).map((c) => c.length))
  const totalH = maxRows * NODE_H + (maxRows - 1) * ROW_GAP + 2 * SVG_PAD
  const totalW = colCount * NODE_W + (colCount - 1) * COL_GAP + 2 * SVG_PAD

  const placed: LayoutNode[] = []
  for (let c = 0; c < cols.length; c++) {
    const colNodes = cols[c]
    if (!colNodes) continue
    const colH = colNodes.length * NODE_H + (colNodes.length - 1) * ROW_GAP
    const startY = SVG_PAD + (totalH - 2 * SVG_PAD - colH) / 2
    for (const n of colNodes) {
      n.x = SVG_PAD + c * (NODE_W + COL_GAP)
      n.y = startY + n.row * (NODE_H + ROW_GAP)
      placed.push(n)
    }
  }

  const edges: LayoutEdge[] = topology.edges.map((e) => ({
    from: nodeId(e.from_ip, e.from_port),
    to: nodeId(e.to_ip, e.to_port),
    kind: e.kind,
    turn_count: e.turn_count,
  }))

  return { placed, edges, totalW, totalH }
}

function NodeCard({ n }: { n: LayoutNode }) {
  const app = n.app ?? "unknown"
  const isClients = n.server_ip === "__clients__"
  const wrapCls =
    APP_NODE_STYLE[app] ?? "border-slate-300 bg-card"
  const badgeCls =
    APP_BADGE_STYLE[app] ??
    "bg-muted text-muted-foreground"
  const topModels = n.models.slice(0, 2)
  return (
    <div
      className={cn(
        "flex h-full w-full flex-col gap-1 rounded-lg border-2 px-2.5 py-1.5 shadow-sm",
        wrapCls,
      )}
      title={
        isClients
          ? `Aggregated client traffic — ${formatNumber(n.call_count)} inbound turn-endpoints`
          : n.models.length > 0
            ? `Models: ${n.models.join(", ")}`
            : undefined
      }
    >
      <div className="flex items-center gap-1.5">
        {isClients ? <Users className="size-3.5 shrink-0" /> : null}
        <span
          className={cn(
            "rounded px-1.5 py-0.5 text-[10px] font-medium",
            isClients ? "bg-slate-200 text-slate-700 dark:bg-slate-700 dark:text-slate-100" : badgeCls,
          )}
        >
          {app}
        </span>
        <span className="ml-auto text-[10px] tabular-nums text-muted-foreground">
          {formatNumber(n.call_count)}
        </span>
      </div>
      {!isClients && (
        <div className="truncate font-mono text-[11px] font-medium">
          {n.server_ip}:{n.server_port}
        </div>
      )}
      {!isClients && topModels.length > 0 && (
        <div className="truncate text-[10px] text-muted-foreground" title={n.models.join(", ")}>
          {topModels.join(", ")}
          {n.models.length > topModels.length ? ` +${n.models.length - topModels.length}` : ""}
        </div>
      )}
      {isClients && (
        <div className="text-[10px] text-muted-foreground">all upstream callers</div>
      )}
    </div>
  )
}

export function ServicePathView({ topology }: { topology: ServicesTopology }) {
  const layout = useMemo(() => layoutGraph(topology), [topology])
  const { placed, edges, totalW, totalH } = layout

  if (placed.length === 0) {
    return (
      <div className="flex h-[400px] items-center justify-center text-sm text-muted-foreground">
        No services observed in selected time range
      </div>
    )
  }

  const posById = new Map<string, LayoutNode>()
  for (const n of placed) posById.set(n.id, n)

  // Scale edge stroke width by turn_count relative to the max so the
  // hottest path stands out without making low-volume edges invisible.
  const maxCount = Math.max(1, ...edges.map((e) => e.turn_count))
  const strokeWidth = (count: number) => {
    const norm = count / maxCount
    return Math.max(1.2, Math.min(6, 1.2 + norm * 4.8))
  }

  return (
    <div className="relative w-full overflow-auto">
      <svg
        width={totalW}
        height={totalH}
        style={{ minWidth: totalW, minHeight: totalH }}
        className="block"
      >
        <defs>
          <marker
            id="arrow-proxy"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="6"
            markerHeight="6"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" className="fill-blue-500" />
          </marker>
          <marker
            id="arrow-client"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="6"
            markerHeight="6"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" className="fill-slate-400" />
          </marker>
        </defs>
        {edges.map((e, idx) => {
          const from = posById.get(e.from)
          const to = posById.get(e.to)
          if (!from || !to) return null
          const x1 = from.x + NODE_W
          const y1 = from.y + NODE_H / 2
          const x2 = to.x
          const y2 = to.y + NODE_H / 2
          // Cubic bezier so the curve looks intentional rather than
          // a straight diagonal that crosses every other node.
          const cx1 = x1 + COL_GAP / 2
          const cx2 = x2 - COL_GAP / 2
          const d = `M ${x1} ${y1} C ${cx1} ${y1}, ${cx2} ${y2}, ${x2} ${y2}`
          const colorCls = e.kind === "proxy" ? "stroke-blue-500" : "stroke-slate-400"
          const dash = e.kind === "client" ? "4 4" : undefined
          return (
            <g key={idx}>
              <path
                d={d}
                fill="none"
                className={colorCls}
                strokeWidth={strokeWidth(e.turn_count)}
                strokeDasharray={dash}
                markerEnd={`url(#${e.kind === "proxy" ? "arrow-proxy" : "arrow-client"})`}
                opacity={0.75}
              />
              {/* Mid-edge label — only on proxy edges where the count
                  is meaningful. Client edges are aggregate and a label
                  on every dotted arrow becomes noise. */}
              {e.kind === "proxy" && (
                <text
                  x={(x1 + x2) / 2}
                  y={(y1 + y2) / 2 - 4}
                  textAnchor="middle"
                  className="fill-blue-700 text-[10px] dark:fill-blue-300"
                >
                  {formatNumber(e.turn_count)}
                </text>
              )}
            </g>
          )
        })}
        {placed.map((n) => (
          <foreignObject key={n.id} x={n.x} y={n.y} width={NODE_W} height={NODE_H}>
            <NodeCard n={n} />
          </foreignObject>
        ))}
      </svg>
      <div className="sticky bottom-0 mt-2 flex items-center gap-4 border-t border-border bg-card px-3 py-2 text-[10px] text-muted-foreground">
        <span className="inline-flex items-center gap-1">
          <span className="inline-block h-0.5 w-4 bg-blue-500"></span>
          proxy hop (paired by sweeper)
        </span>
        <span className="inline-flex items-center gap-1">
          <span
            className="inline-block h-0.5 w-4 bg-slate-400"
            style={{ backgroundImage: "linear-gradient(90deg, currentColor 50%, transparent 50%)", backgroundSize: "8px 1px" }}
          ></span>
          client entry edge
        </span>
        <span className="ml-auto">Edge width ∝ turn count</span>
      </div>
    </div>
  )
}
