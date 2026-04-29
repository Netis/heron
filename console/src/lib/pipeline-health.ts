import type { MetricRecord } from "@/types/api"

export type HealthLevel = "healthy" | "warning" | "critical"

export type HealthFinding = {
  level: "warning" | "critical"
  metric: string
  message: string
}

export type Health = {
  level: HealthLevel
  findings: HealthFinding[]
}

export const CRITICAL_DELTA_COUNTERS = new Set([
  "pkts_dropped_kernel",
  "flush_errors",
  "read_errors",
  "dump_errors",
  "batches_dropped_zmq",
])

export const WARNING_DELTA_COUNTERS = new Set([
  "tcp_ooo_dropped",
  "http_resyncs",
  "turns_discarded_no_user_start",
  "calls_dropped_late",
  "heartbeats_dropped",
])

export const WARNING_THRESHOLD = 0.9
export const CRITICAL_THRESHOLD = 0.95

/**
 * Classify the current snapshot into healthy/warning/critical.
 *
 * `prev` is a `metric_name -> previous_value` lookup used to detect deltas.
 * Pass `{}` on the first frame — no critical/warning will be produced for
 * delta-based rules until a second frame arrives, but cumulative rules
 * still fire (e.g. tcp_ooo_dropped > 0 stays warning even on first frame).
 */
export function classifyHealth(
  metrics: MetricRecord[],
  prev: Record<string, number>,
): Health {
  const findings: HealthFinding[] = []

  for (const m of metrics) {
    const delta = m.value - (prev[m.name] ?? m.value)

    if (m.kind === "counter") {
      if (CRITICAL_DELTA_COUNTERS.has(m.name) && delta > 0) {
        findings.push({
          level: "critical",
          metric: m.name,
          message: `${m.name} +${delta} since last sample`,
        })
      } else if (WARNING_DELTA_COUNTERS.has(m.name)) {
        if (delta > 0) {
          findings.push({
            level: "warning",
            metric: m.name,
            message: `${m.name} +${delta} since last sample`,
          })
        } else if (m.value > 0) {
          findings.push({
            level: "warning",
            metric: m.name,
            message: `${m.name} cumulative ${m.value} (no recent change)`,
          })
        }
      }
    }

    if (m.kind === "gauge" && m.capacity && m.capacity > 0) {
      const ratio = m.value / m.capacity
      if (ratio >= CRITICAL_THRESHOLD) {
        findings.push({
          level: "critical",
          metric: m.name,
          message: `${m.name} ${m.value}/${m.capacity} (${Math.round(ratio * 100)}%)`,
        })
      } else if (ratio >= WARNING_THRESHOLD) {
        findings.push({
          level: "warning",
          metric: m.name,
          message: `${m.name} ${m.value}/${m.capacity} (${Math.round(ratio * 100)}%)`,
        })
      }
    }
  }

  const level: HealthLevel = findings.some((f) => f.level === "critical")
    ? "critical"
    : findings.length > 0
      ? "warning"
      : "healthy"

  return { level, findings }
}

export type FunnelStageLabel =
  | "pkts_received"
  | "pkts_parsed"
  | "http_exchanges_joined"
  | "wires_detected"
  | "calls_with_agent"
  | "calls_ingested"
  | "turns_completed"
  | "flushed_calls"

export type FunnelStageSpec = {
  label: FunnelStageLabel
  /** The metric name that supplies `value` for this row. */
  source: string
  /** Annotation generator — given the snapshot map, produce a drop note. */
  annotate: (snap: Record<string, number>) => string | null
}

export const FUNNEL_STAGES: FunnelStageSpec[] = [
  {
    label: "pkts_received",
    source: "pkts_received",
    annotate: () => null,
  },
  {
    label: "pkts_parsed",
    source: "pkts_parsed",
    annotate: (snap) => {
      const not_ip = snap.pkts_dropped_not_ip ?? 0
      const not_tcp = snap.pkts_dropped_not_tcp ?? 0
      const malformed = snap.pkts_dropped_malformed ?? 0
      const total = not_ip + not_tcp + malformed
      return `-${total} (not_ip ${not_ip}, not_tcp ${not_tcp}, malformed ${malformed})`
    },
  },
  {
    label: "http_exchanges_joined",
    source: "http_exchanges_joined",
    annotate: (snap) => {
      const unpaired = snap.http_exchanges_unpaired ?? 0
      const expired = snap.http_exchanges_expired ?? 0
      return `subset that is HTTP; -${
        unpaired + expired
      } (unpaired ${unpaired}, expired ${expired})`
    },
  },
  {
    label: "wires_detected",
    source: "wires_detected",
    annotate: (snap) => {
      const ignored = snap.wires_ignored ?? 0
      return `subset matching an LLM wire-API; rest are wires_ignored ${ignored}`
    },
  },
  {
    label: "calls_with_agent",
    source: "calls_with_agent",
    annotate: (snap) => `-${snap.calls_without_agent ?? 0} calls_without_agent`,
  },
  {
    label: "calls_ingested",
    source: "calls_ingested",
    annotate: (snap) => {
      const dropped_late = snap.calls_dropped_late ?? 0
      const aux = snap.calls_auxiliary ?? 0
      return `-${dropped_late} dropped_late, +${aux} auxiliary (not part of any turn)`
    },
  },
  {
    label: "turns_completed",
    source: "turns_completed",
    annotate: () => "fan-in: multiple calls per turn — this is not a drop",
  },
  {
    label: "flushed_calls",
    source: "flushed_calls",
    annotate: (snap) => {
      const buf = snap.buf_calls ?? 0
      const flushed = snap.flushed_calls ?? 0
      const not_flushed = Math.max(0, buf - flushed)
      return `${not_flushed} not yet flushed`
    },
  },
]

export type FunnelRow = {
  label: FunnelStageLabel
  value: number
  widthRatio: number
  dropAnnotation: string | null
}

export function computeFunnel(metrics: MetricRecord[]): FunnelRow[] {
  const snap: Record<string, number> = {}
  for (const m of metrics) snap[m.name] = m.value

  const root = snap.pkts_received ?? 0

  return FUNNEL_STAGES.map((stage) => {
    const value = snap[stage.source] ?? 0
    const widthRatio = root > 0 ? value / root : 0
    return {
      label: stage.label,
      value,
      widthRatio,
      dropAnnotation: stage.annotate(snap),
    }
  })
}
