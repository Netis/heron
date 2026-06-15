import { cn } from "@/lib/utils"
import { useRuntimeConfig } from "@/hooks/use-runtime-config"
import type { MetricRecord } from "@/types/api"

type Props = {
  /** Active pipeline's metrics (the eBPF source registers under the pipeline). */
  pipelineMetrics: MetricRecord[]
  /** Previous frame's values, by metric name — used to tell "capturing now". */
  prevByName: Record<string, number>
}

function val(metrics: MetricRecord[], name: string): number {
  return metrics.find((m) => m.name === name)?.value ?? 0
}

function Tile({
  label,
  value,
  warn = false,
}: {
  label: string
  value: number
  warn?: boolean
}) {
  return (
    <div className="flex min-w-[110px] flex-col gap-0.5 rounded-md border border-border bg-muted/30 px-2.5 py-1.5">
      <span className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      <div
        className={cn(
          "text-base font-bold tabular-nums",
          warn ? "text-red-600 dark:text-red-400" : "text-foreground",
        )}
      >
        {value.toLocaleString()}
      </div>
    </div>
  )
}

/**
 * Dedicated, at-a-glance status for the on-host eBPF SSL-uprobe capture source.
 * Answers "is eBPF actually working on this instance?" without reading the raw
 * metric table: a green dot + "Capturing" means uprobes are attached to libssl
 * and TLS plaintext is flowing through the pipeline right now.
 */
export function EbpfStatusSection({ pipelineMetrics, prevByName }: Props) {
  const config = useRuntimeConfig()
  const available = config.data?.ebpf_available ?? false

  const uprobes = val(pipelineMetrics, "ebpf_uprobes_attached")
  const events = val(pipelineMetrics, "ebpf_events_received")
  const dropped = val(pipelineMetrics, "ebpf_events_dropped")
  const bytes = val(pipelineMetrics, "ebpf_bytes_captured")
  const frames = val(pipelineMetrics, "ebpf_frames_synthesized")
  const conns = val(pipelineMetrics, "ebpf_connections_active")
  const procs = val(pipelineMetrics, "ebpf_process_cache_size")

  // "Capturing now" = events advanced since the previous polled frame.
  const prevEvents = prevByName["ebpf_events_received"] ?? events
  const capturingNow = events > prevEvents

  let tone: "live" | "idle" | "warn" | "off"
  let label: string
  let detail: string
  if (!available) {
    tone = "off"
    label = "Unavailable"
    detail =
      "This binary was built without the `ebpf` feature — on-host SSL-uprobe capture is not compiled in. Deploy an eBPF-enabled build to use it."
  } else if (uprobes < 1) {
    tone = "warn"
    label = "No uprobes attached"
    detail =
      "eBPF is available and an `ebpf` source is configured, but no uprobes are attached. Check CAP_BPF / CAP_PERFMON / CAP_SYS_ADMIN on the service and that libssl was found on the host."
  } else if (capturingNow) {
    tone = "live"
    label = "Capturing"
    detail = `Live — TLS plaintext is flowing through ${uprobes} uprobe${
      uprobes === 1 ? "" : "s"
    } on libssl right now.`
  } else if (events > 0) {
    tone = "idle"
    label = "Attached · idle"
    detail = `Uprobes attached to libssl; ${events.toLocaleString()} SSL events captured so far. No traffic in the last frame.`
  } else {
    tone = "idle"
    label = "Attached · waiting"
    detail =
      "Uprobes attached, but no SSL events captured yet. Generate TLS traffic through libssl (e.g. an `openssl s_client` request) to confirm end to end."
  }

  const dot = {
    live: "bg-green-500",
    idle: "bg-amber-500",
    warn: "bg-red-500",
    off: "bg-muted-foreground/50",
  }[tone]
  const badge = {
    live: "border-green-600/40 bg-green-600/10 text-green-700 dark:text-green-400",
    idle: "border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-400",
    warn: "border-red-600/40 bg-red-600/10 text-red-700 dark:text-red-400",
    off: "border-border bg-muted/40 text-muted-foreground",
  }[tone]

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <span className={cn("inline-block size-2.5 rounded-full", dot)} />
        <h3 className="text-xs font-bold uppercase tracking-wider text-muted-foreground">
          eBPF SSL-uprobe capture
        </h3>
        <span
          className={cn(
            "rounded-md border px-2 py-0.5 text-xs font-semibold",
            badge,
          )}
        >
          {label}
        </span>
        {available && uprobes >= 1 ? (
          <span className="text-xs text-muted-foreground">
            {uprobes} uprobe{uprobes === 1 ? "" : "s"} attached
          </span>
        ) : null}
      </div>
      <p className="mb-3 text-xs text-muted-foreground">{detail}</p>
      {available ? (
        <div className="flex flex-wrap gap-2">
          <Tile label="events received" value={events} />
          <Tile label="bytes captured" value={bytes} />
          <Tile label="frames synth" value={frames} />
          <Tile label="active conns" value={conns} />
          <Tile label="process cache" value={procs} />
          <Tile label="events dropped" value={dropped} warn={dropped > 0} />
        </div>
      ) : null}
    </section>
  )
}
