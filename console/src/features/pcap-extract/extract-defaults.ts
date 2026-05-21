// Pure mappers from anchor rows to pcap extract request values.

import type { LlmCallDetail, HttpExchangeDetail, AgentTurnDetail } from "@/types/api"

export interface ExtractFormValues {
  source_id: string          // read-only
  client_ip: string
  client_port: string
  server_ip: string
  server_port: string
  start_us: number           // microseconds since epoch (matches API)
  end_us: number
}

export type Anchor =
  | { type: "http_exchange"; row: HttpExchangeDetail }
  | { type: "llm_call"; row: LlmCallDetail }
  | { type: "agent_turn"; row: AgentTurnDetail }

export type ExtractAnchor = Exclude<Anchor, { type: "agent_turn" }>

const SECOND_US = 1_000_000

/// Convert an ISO-or-ms timestamp from the API to microseconds since epoch.
/// API timestamps are documented as ms; LLM-call complete_time is sometimes
/// missing (still in flight) — caller must guard.
function tsToUs(ts_ms: number): number {
  return ts_ms * 1000
}

export function defaultsFor(anchor: ExtractAnchor): ExtractFormValues {
  switch (anchor.type) {
    case "http_exchange": {
      const r = anchor.row
      const start_us = tsToUs(r.request_time) - SECOND_US
      const end_us = r.response_complete_time != null
        ? tsToUs(r.response_complete_time) + SECOND_US
        : tsToUs(r.request_time) + 5 * SECOND_US
      return {
        source_id: r.source_id ?? "",
        client_ip: r.client_ip ?? "",
        client_port: r.client_port?.toString() ?? "",
        server_ip: r.server_ip ?? "",
        server_port: r.server_port?.toString() ?? "",
        start_us,
        end_us,
      }
    }
    case "llm_call": {
      const r = anchor.row
      const end_ms = r.complete_time ?? r.response_time ?? (r.request_time + 5_000)
      return {
        source_id: r.source_id ?? "",
        client_ip: r.client_ip ?? "",
        client_port: r.client_port?.toString() ?? "",
        server_ip: r.server_ip ?? "",
        server_port: r.server_port?.toString() ?? "",
        start_us: tsToUs(r.request_time) - SECOND_US,
        end_us: tsToUs(end_ms) + SECOND_US,
      }
    }
  }
}

const ONE_HOUR_US = 60 * 60 * 1_000_000

export interface FormValidation {
  ok: boolean
  reason?: string
}

export function validate(v: ExtractFormValues): FormValidation {
  if (v.start_us >= v.end_us) return { ok: false, reason: "start must be < end" }
  if (v.end_us - v.start_us > ONE_HOUR_US) return { ok: false, reason: "time window > 1h" }
  if (v.client_ip && !looksLikeIp(v.client_ip)) return { ok: false, reason: "client_ip is malformed" }
  if (v.server_ip && !looksLikeIp(v.server_ip)) return { ok: false, reason: "server_ip is malformed" }
  if (v.client_port && !looksLikePort(v.client_port)) return { ok: false, reason: "client_port is malformed" }
  if (v.server_port && !looksLikePort(v.server_port)) return { ok: false, reason: "server_port is malformed" }
  return { ok: true }
}

function looksLikeIp(s: string): boolean {
  // Cheap: IPv4 or IPv6 surface check; full validation happens server-side.
  return /^(\d{1,3}\.){3}\d{1,3}$|^[\da-fA-F:]+$/.test(s)
}
function looksLikePort(s: string): boolean {
  const n = Number(s)
  return Number.isInteger(n) && n >= 0 && n <= 65535
}

export function buildExtractUrl(v: ExtractFormValues): string {
  const params = new URLSearchParams()
  params.set("source_id", v.source_id)
  params.set("start", v.start_us.toString())
  params.set("end", v.end_us.toString())
  if (v.client_ip)   params.set("client_ip", v.client_ip)
  if (v.client_port) params.set("client_port", v.client_port)
  if (v.server_ip)   params.set("server_ip", v.server_ip)
  if (v.server_port) params.set("server_port", v.server_port)
  return `/api/pcap/extract?${params.toString()}`
}

export function buildAgentTurnPacketsUrl(turn: AgentTurnDetail): string {
  return `/api/pcap/agent-turns/${encodeURIComponent(turn.turn_id)}/packets`
}
