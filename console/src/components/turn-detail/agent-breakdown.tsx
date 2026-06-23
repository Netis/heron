import { ToolSurfacePill, TopologyPill } from "@/components/agent-pills"
import type { AgentTurnDetail, AgentTurnCallItem } from "@/types/api"

interface Props {
  turn: AgentTurnDetail
  calls: AgentTurnCallItem[]
}

export function AgentBreakdown({ turn, calls }: Props) {
  const subAgentCount = calls.filter((c) => c.agent_topology === "sub_agent").length

  return (
    <section className="space-y-1 border-t border-border px-4 py-3 text-sm">
      <h3 className="text-xs uppercase tracking-wide text-muted-foreground">Agent breakdown</h3>
      <div className="grid grid-cols-[120px_1fr] gap-x-3 gap-y-1.5">
        <div className="text-muted-foreground">Topology</div>
        <div className="flex items-center gap-2">
          {turn.agent_topology ? (
            <TopologyPill topology={turn.agent_topology} />
          ) : (
            <span className="text-xs text-muted-foreground">—</span>
          )}
          {turn.agent_topology === "orchestrator" && subAgentCount > 0 && (
            <span className="text-xs text-muted-foreground">
              → {subAgentCount} sub-agent{subAgentCount === 1 ? "" : "s"}
            </span>
          )}
        </div>

        <div className="text-muted-foreground">Tool surfaces</div>
        <div className="flex flex-wrap gap-1">
          {turn.tool_surfaces.length > 0 ? (
            turn.tool_surfaces.map((s) => <ToolSurfacePill key={s} surface={s} />)
          ) : (
            <span className="text-xs text-muted-foreground">none</span>
          )}
        </div>

        <div className="text-muted-foreground">Tool calls</div>
        <div>
          {turn.tool_call_total} total across {turn.span_ids?.length ?? 0} call
          {(turn.span_ids?.length ?? 0) === 1 ? "" : "s"}
        </div>

        {turn.suspicious_skills.length > 0 && (
          <>
            <div className="text-muted-foreground">Suspicious</div>
            <ul className="space-y-0.5 text-xs">
              {turn.suspicious_skills.map((s) => (
                <li key={s.tool_name}>
                  <code className="rounded bg-muted px-1 py-0.5">{s.tool_name}</code>{" "}
                  <span className="text-muted-foreground">({s.reason})</span>
                </li>
              ))}
            </ul>
          </>
        )}
      </div>
    </section>
  )
}
