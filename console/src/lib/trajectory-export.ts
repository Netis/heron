/** URL builders for the trajectory-export endpoints. */

export interface BatchExportFilters {
  /** Seconds since epoch (toolbar window). */
  start: number
  end: number
  wire_api?: string
  model?: string
  server_ip?: string
  status?: string
  agent_kind?: string
  client_ip?: string
  server_port?: string
  include_proxy_hops?: boolean
}

/** Single turn → one trajectory. */
export function turnTrajectoryUrl(turnId: string): string {
  const p = new URLSearchParams({ scope: "turn", turn_id: turnId })
  return `/api/export/trajectory?${p.toString()}`
}

/** Single session → one (multi-turn) trajectory. */
export function sessionTrajectoryUrl(sourceId: string, sessionId: string): string {
  const p = new URLSearchParams({ scope: "session", source_id: sourceId, session_id: sessionId })
  return `/api/export/trajectory?${p.toString()}`
}

/** Batch → one trajectory per turn matching the agent-turns filters. */
export function batchTrajectoriesUrl(f: BatchExportFilters): string {
  const p = new URLSearchParams()
  p.set("start", String(f.start))
  p.set("end", String(f.end))
  const optional: Record<string, string | undefined> = {
    wire_api: f.wire_api,
    model: f.model,
    server_ip: f.server_ip,
    status: f.status,
    agent_kind: f.agent_kind,
    client_ip: f.client_ip,
    server_port: f.server_port,
  }
  for (const [k, v] of Object.entries(optional)) {
    if (v) p.set(k, v)
  }
  if (f.include_proxy_hops) p.set("include_proxy_hops", "true")
  return `/api/export/trajectories?${p.toString()}`
}
