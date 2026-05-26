import type { ToolSurface, AgentTopology } from "@/types/api"

const SURFACE_LABEL: Record<ToolSurface, string> = {
  function_call: "function",
  mcp: "mcp",
  cli: "cli",
  mixed: "mixed",
  unknown: "?",
}

export function ToolSurfacePill({ surface }: { surface: ToolSurface }) {
  return (
    <span className="inline-flex items-center rounded-md bg-muted px-1.5 py-0.5 text-xs font-mono">
      {SURFACE_LABEL[surface]}
    </span>
  )
}

const TOPOLOGY_LABEL: Record<AgentTopology, string> = {
  single_agent: "single",
  sub_agent: "sub-agent",
  orchestrator: "orchestrator",
}

export function TopologyPill({ topology }: { topology: AgentTopology }) {
  return (
    <span className="inline-flex items-center rounded-md bg-secondary px-1.5 py-0.5 text-xs">
      {TOPOLOGY_LABEL[topology]}
    </span>
  )
}

export function SuspiciousMarker({ count }: { count: number }) {
  if (count === 0) return null
  return (
    <span
      className="inline-flex h-5 w-5 items-center justify-center rounded-full bg-amber-100 text-amber-900 text-xs"
      title={`${count} suspicious skill${count === 1 ? "" : "s"}`}
    >
      ⚠
    </span>
  )
}
