import {
  CRITICAL_DELTA_COUNTERS,
  WARNING_DELTA_COUNTERS,
} from "@/lib/pipeline-health"
import { cn } from "@/lib/utils"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
  prevByName: Record<string, number>
}

const EXPLANATIONS: Record<string, string> = {
  pkts_dropped_kernel:
    "Kernel ring buffer overflowed — capture is falling behind. Increase buffer size or reduce filter scope.",
  flush_errors: "Storage backend rejected a flush. Check the storage logs.",
  read_errors: "Pcap source returned an error during read.",
  dump_errors: "Packet dumper failed to write a frame to disk.",
  batches_dropped_zmq: "ZMQ batches dropped due to HWM. Receiver is slower than the probe.",
  tcp_ooo_dropped: "TCP segment received out of order and exceeded the buffer.",
  http_resyncs: "HTTP parser had to resync — typically due to a snaplen-truncated frame.",
  turns_discarded_no_user_start:
    "A call was assigned an agent but no user-message start was ever seen — typically mid-stream capture.",
  calls_dropped_late: "Call arrived after its turn was finalized — partition timing issue.",
  heartbeats_dropped:
    "Capture-source heartbeat could not be enqueued (channel full).",
}

type Finding = {
  level: "critical" | "warning"
  metric: string
  value: number
  delta: number
}

function buildFindings(
  metrics: MetricRecord[],
  prev: Record<string, number>,
): Finding[] {
  const out: Finding[] = []
  for (const m of metrics) {
    if (m.kind !== "counter") continue
    const delta = Math.max(0, m.value - (prev[m.name] ?? m.value))
    if (CRITICAL_DELTA_COUNTERS.has(m.name)) {
      if (m.value > 0 || delta > 0) {
        out.push({ level: "critical", metric: m.name, value: m.value, delta })
      }
    } else if (WARNING_DELTA_COUNTERS.has(m.name)) {
      if (m.value > 0 || delta > 0) {
        out.push({ level: "warning", metric: m.name, value: m.value, delta })
      }
    }
  }
  // critical first, then by largest delta
  return out.sort(
    (a, b) =>
      Number(b.level === "critical") - Number(a.level === "critical") ||
      b.delta - a.delta,
  )
}

export function ErrorListSection({
  pipelineMetrics,
  globalMetrics,
  prevByName,
}: Props) {
  const findings = buildFindings(
    [...pipelineMetrics, ...globalMetrics],
    prevByName,
  )

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ④ Errors
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Counters that should normally be zero. Anything here is worth a look.
      </p>
      {findings.length === 0 ? (
        <div className="rounded-md border border-emerald-300 bg-emerald-50 p-2 text-sm text-emerald-700 dark:border-emerald-600 dark:bg-emerald-950/40 dark:text-emerald-300">
          ✓ No errors recorded.
        </div>
      ) : (
        <div className="flex flex-col gap-1.5">
          {findings.map((f) => (
            <div
              key={f.metric}
              className={cn(
                "flex items-start gap-3 rounded-md border p-2",
                f.level === "critical"
                  ? "border-red-300 bg-red-50 dark:border-red-700 dark:bg-red-950/40"
                  : "border-amber-300 bg-amber-50 dark:border-amber-700 dark:bg-amber-950/40",
              )}
            >
              <span
                className={cn(
                  "mt-0.5 inline-block w-12 shrink-0 text-center text-[10px] font-bold uppercase",
                  f.level === "critical"
                    ? "text-red-700 dark:text-red-300"
                    : "text-amber-700 dark:text-amber-300",
                )}
              >
                {f.level}
              </span>
              <div className="flex-1">
                <div className="font-mono text-sm">
                  {f.metric}{" "}
                  <span className="text-muted-foreground">
                    = {f.value.toLocaleString()} (Δ +{f.delta})
                  </span>
                </div>
                <div className="mt-0.5 text-xs text-muted-foreground">
                  {EXPLANATIONS[f.metric] ?? ""}
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
    </section>
  )
}
