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
  | "pkts_routed"
  | "pkts_parsed"
  | "http_reqs_parsed"
  | "http_resps_parsed"
  | "http_exchanges_joined"
  | "wires_detected"
  | "calls_with_agent"
  | "calls_ingested"
  | "turns_completed"
  | "windows_emitted"
  | "flushed_calls"
  | "flushed_turns"
  | "flushed_exchanges"
  | "flushed_metrics"

/**
 * The pipeline stage that owns a funnel row. Mirrors `MetricGroup` but with
 * sub-stages spelled out (`net` / `joiner` / `http`) where the same group
 * covers multiple distinct steps along the flow.
 */
export type FunnelStageName =
  | "capture"
  | "dispatcher"
  | "net"
  | "http"
  | "joiner"
  | "llm"
  | "turn"
  | "metrics"
  | "storage"

/**
 * What kind of comparison this row represents — drives bar rendering.
 *
 * - `root`     — funnel root (`pkts_received`). Bar at 100% of itself; the
 *                "% of <upstream>" caption is suppressed.
 * - `normal`   — passthrough; bar = value/upstream, expected near 100%. A
 *                low ratio means data was lost at this hop.
 * - `filter`   — passthrough where a low ratio is expected (e.g.
 *                `wires_detected`: most HTTP isn't LLM). Renderer paints
 *                this gray so the eye doesn't read it as "loss".
 * - `info`     — no comparable upstream (units differ, or the row is just
 *                an emission count). No bar, no percentage; only the
 *                absolute value + annotation.
 * - `fanIn`    — value is structurally smaller than upstream by design
 *                (e.g. `turns_completed`: many calls per turn). Bar
 *                suppressed; annotation explains.
 */
export type FunnelRowKind = "root" | "normal" | "filter" | "info" | "fanIn"

export type FunnelStageSpec = {
  label: FunnelStageLabel
  /** Pipeline stage owning this row — used to group/label the funnel. */
  stage: FunnelStageName
  /** The metric name that supplies `value` for this row. */
  source: string
  /** Row kind — see `FunnelRowKind`. */
  kind: FunnelRowKind
  /**
   * The metric whose value this row should be ratio'd against for the bar.
   * Required for `normal` and `filter`. Must be `null` for `root`, `info`,
   * `fanIn` (those rows don't render a per-hop ratio).
   */
  upstream: string | null
  /** Annotation generator — given the snapshot map, produce a drop note. */
  annotate: (snap: Record<string, number>) => string | null
}

// Annotation helper for the storage block: report buffer lag for an entity.
// `bufMetric` is the WriteBuffer's instantaneous pending-batch gauge — the
// current count of items that have been received but not yet flushed.
const flushLag = (
  bufMetric: string,
  fromStage: string,
) => (snap: Record<string, number>) => {
  const pending = snap[bufMetric] ?? 0
  return `from ${fromStage}; ${pending} still buffered (not yet written)`
}

export const FUNNEL_STAGES: FunnelStageSpec[] = [
  {
    label: "pkts_received",
    stage: "capture",
    source: "pkts_received",
    kind: "root",
    upstream: null,
    annotate: () => null,
  },
  {
    label: "pkts_routed",
    stage: "dispatcher",
    source: "pkts_routed",
    kind: "normal",
    upstream: "pkts_received",
    annotate: (snap) => {
      const dropped = (snap.pkts_received ?? 0) - (snap.pkts_routed ?? 0)
      return dropped > 0
        ? `-${dropped} not routed (dispatcher saturated)`
        : null
    },
  },
  {
    label: "pkts_parsed",
    stage: "net",
    source: "pkts_parsed",
    kind: "normal",
    upstream: "pkts_routed",
    annotate: (snap) => {
      const not_ip = snap.pkts_dropped_not_ip ?? 0
      const not_tcp = snap.pkts_dropped_not_tcp ?? 0
      const malformed = snap.pkts_dropped_malformed ?? 0
      const total = not_ip + not_tcp + malformed
      return `-${total} (not_ip ${not_ip}, not_tcp ${not_tcp}, malformed ${malformed})`
    },
  },
  {
    label: "http_reqs_parsed",
    stage: "http",
    source: "http_reqs_parsed",
    kind: "info",
    upstream: null,
    annotate: () => "HTTP request boundaries reconstructed from TCP streams",
  },
  {
    label: "http_resps_parsed",
    stage: "http",
    source: "http_resps_parsed",
    kind: "normal",
    upstream: "http_reqs_parsed",
    annotate: () => "request:response should be ≈1:1 in steady state",
  },
  {
    label: "http_exchanges_joined",
    stage: "joiner",
    source: "http_exchanges_joined",
    kind: "normal",
    upstream: "http_resps_parsed",
    annotate: (snap) => {
      const unpaired = snap.http_exchanges_unpaired ?? 0
      const expired = snap.http_exchanges_expired ?? 0
      return `-${
        unpaired + expired
      } (unpaired ${unpaired}, expired ${expired})`
    },
  },
  {
    label: "wires_detected",
    stage: "llm",
    source: "wires_detected",
    kind: "filter",
    upstream: "http_exchanges_joined",
    annotate: (snap) => {
      const ignored = snap.wires_ignored ?? 0
      return `filter: only LLM-wire HTTP traffic survives; rest are wires_ignored ${ignored}`
    },
  },
  {
    label: "calls_with_agent",
    stage: "llm",
    source: "calls_with_agent",
    kind: "normal",
    upstream: "wires_detected",
    annotate: (snap) => `-${snap.calls_without_agent ?? 0} calls_without_agent`,
  },
  {
    label: "calls_ingested",
    stage: "turn",
    source: "calls_ingested",
    kind: "normal",
    upstream: "calls_with_agent",
    annotate: (snap) => {
      const dropped_late = snap.calls_dropped_late ?? 0
      const aux = snap.calls_auxiliary ?? 0
      return `-${dropped_late} dropped_late, +${aux} auxiliary (not part of any turn)`
    },
  },
  {
    label: "turns_completed",
    stage: "turn",
    source: "turns_completed",
    kind: "fanIn",
    upstream: null,
    annotate: () => "fan-in: multiple calls per turn — this is not a drop",
  },
  {
    label: "windows_emitted",
    stage: "metrics",
    source: "windows_emitted",
    kind: "info",
    upstream: null,
    annotate: () =>
      "aggregation windows emitted by metrics stage (one window expands into N rows)",
  },
  // -- Storage block ---------------------------------------------------------
  // Four independent sinks, each tied back to the producer that feeds it.
  // Bars are "% of producer that reached storage" — near 100% in steady state.
  // `flushed_metrics` is `info` (windows ≠ rows; ratio would be misleading).
  {
    label: "flushed_calls",
    stage: "storage",
    source: "flushed_calls",
    kind: "normal",
    upstream: "calls_with_agent",
    annotate: flushLag("buf_calls", "llm (direct)"),
  },
  {
    label: "flushed_turns",
    stage: "storage",
    source: "flushed_turns",
    kind: "normal",
    upstream: "turns_completed",
    annotate: flushLag("buf_turns", "turn tracker"),
  },
  {
    label: "flushed_exchanges",
    stage: "storage",
    source: "flushed_exchanges",
    kind: "normal",
    upstream: "http_exchanges_joined",
    annotate: flushLag("buf_exchanges", "joiner"),
  },
  {
    label: "flushed_metrics",
    stage: "storage",
    source: "flushed_metrics",
    kind: "info",
    upstream: null,
    annotate: flushLag("buf_metrics", "metrics aggregator"),
  },
]

export type FunnelRow = {
  label: FunnelStageLabel
  stage: FunnelStageName
  kind: FunnelRowKind
  value: number
  /**
   * Bar fill ratio in [0, 1] for `root`/`normal`/`filter` rows. `null` for
   * `info` and `fanIn` (renderer suppresses the bar and percentage entirely).
   */
  widthRatio: number | null
  /**
   * The metric this row's bar is ratio'd against, surfaced so the renderer
   * can show "X% of <upstream>". `null` for rows without a meaningful basis
   * (`root`, `info`, `fanIn`).
   */
  upstream: string | null
  dropAnnotation: string | null
}

export function computeFunnel(metrics: MetricRecord[]): FunnelRow[] {
  const snap: Record<string, number> = {}
  for (const m of metrics) snap[m.name] = m.value

  return FUNNEL_STAGES.map((spec) => {
    const value = snap[spec.source] ?? 0
    let widthRatio: number | null
    switch (spec.kind) {
      case "root":
        widthRatio = value > 0 ? 1 : 0
        break
      case "normal":
      case "filter": {
        const up = spec.upstream ? (snap[spec.upstream] ?? 0) : 0
        widthRatio = up > 0 ? value / up : 0
        break
      }
      case "info":
      case "fanIn":
        widthRatio = null
        break
    }
    return {
      label: spec.label,
      stage: spec.stage,
      kind: spec.kind,
      value,
      widthRatio,
      upstream: spec.upstream,
      dropAnnotation: spec.annotate(snap),
    }
  })
}
