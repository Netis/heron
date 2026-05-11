import type { ReactNode } from "react"
import { Loader2, RefreshCw, Cpu, Network, HardDrive } from "lucide-react"
import { useRuntimeConfig } from "@/hooks/use-runtime-config"
import { useInternalMetrics } from "@/hooks/use-internal-metrics"
import { useCaptureInterfaces } from "@/hooks/use-capture-interfaces"
import type {
  AppConfigShape,
  CaptureInterface,
  CaptureSource,
  MetricRecord,
  PipelineShape,
} from "@/types/api"

/**
 * Settings page — read-only view of the capture configuration the running
 * tokenscope process is using, with live per-pipeline counters merged in.
 *
 * Sources are the boot-time `[[pipeline.sources]]` entries. They cannot be
 * changed without a process restart; edits land in a follow-up phase.
 */
export function SettingsPage() {
  const config = useRuntimeConfig()
  const metrics = useInternalMetrics()
  const interfaces = useCaptureInterfaces()

  const isInitialLoad =
    config.isLoading || (metrics.isLoading && !metrics.data) || interfaces.isLoading

  if (isInitialLoad) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  if (!config.data) {
    return (
      <div className="flex h-full items-center justify-center p-6 text-sm text-destructive">
        Failed to load runtime config: {String(config.error ?? "unknown error")}
      </div>
    )
  }

  const appConfig = config.data.config as AppConfigShape
  const pipelines = appConfig.pipelines ?? []
  const metricsByPipeline = buildMetricIndex(metrics.data?.pipelines ?? [])

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* ===== Header ===== */}
      <div className="flex flex-wrap items-center gap-3 rounded-lg border border-border bg-card p-3">
        <span className="text-sm font-semibold">Settings</span>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span>
            version <span className="font-mono text-foreground">{config.data.version}</span>
          </span>
          <span className="break-all">
            config <span className="font-mono text-foreground">{config.data.config_path}</span>
          </span>
        </div>
        <button
          onClick={() => {
            config.refetch()
            metrics.refetch()
            interfaces.refetch()
          }}
          className="ml-auto inline-flex items-center gap-1.5 rounded-md border border-border bg-background px-2.5 py-1 text-xs font-medium hover:bg-muted"
        >
          <RefreshCw className={config.isFetching ? "size-3.5 animate-spin" : "size-3.5"} />
          Refresh
        </button>
      </div>

      <p className="text-xs text-muted-foreground">
        Capture sources are configured at process startup. To change interface or BPF
        filter today, edit{" "}
        <span className="font-mono text-foreground">{config.data.config_path}</span> and
        restart tokenscope. An in-app editor is shipping in a follow-up.
      </p>

      {/* ===== Pipelines ===== */}
      {pipelines.length === 0 ? (
        <div className="rounded-lg border border-border bg-card p-4 text-sm text-muted-foreground">
          No pipelines configured. Tokenscope may be running in CLI mode
          (--pcap-file / -i overrides the config).
        </div>
      ) : (
        pipelines.map((p) => (
          <PipelineCard
            key={p.name}
            pipeline={p}
            metrics={metricsByPipeline.get(p.name) ?? {}}
          />
        ))
      )}

      {/* ===== Available interfaces ===== */}
      <InterfacesCard
        interfaces={interfaces.data?.interfaces ?? []}
        error={interfaces.error}
      />
    </div>
  )
}

// ============================================================================
// PipelineCard
// ============================================================================

function PipelineCard({
  pipeline,
  metrics,
}: {
  pipeline: PipelineShape
  metrics: Record<string, number>
}) {
  return (
    <div className="rounded-lg border border-border bg-card">
      <div className="flex items-center gap-2 border-b border-border px-4 py-2.5">
        <Cpu className="size-4 text-muted-foreground" />
        <span className="text-sm font-semibold">{pipeline.name}</span>
        {pipeline.dispatcher_count !== undefined && (
          <span className="ml-2 text-xs text-muted-foreground">
            {pipeline.dispatcher_count} dispatcher · {pipeline.flow_shard_count ?? "?"} flow shards
          </span>
        )}
      </div>

      {/* Sources */}
      <div className="border-b border-border px-4 py-3">
        <div className="mb-2 text-xs font-medium text-muted-foreground">Sources</div>
        {pipeline.sources.length === 0 ? (
          <div className="text-xs italic text-muted-foreground">no sources</div>
        ) : (
          <ul className="flex flex-col gap-2">
            {pipeline.sources.map((s, i) => (
              <SourceRow key={i} source={s} />
            ))}
          </ul>
        )}
      </div>

      {/* Live counters — capture stage */}
      <div className="border-b border-border px-4 py-3">
        <div className="mb-2 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
          <Network className="size-3.5" /> Live capture counters
        </div>
        <CountersGrid
          metrics={metrics}
          keys={[
            "pkts_received",
            "pkts_dropped_kernel",
            "pkts_truncated",
            "read_errors",
            "heartbeats_emitted",
            "batches_received",
            "batches_dropped_zmq",
          ]}
        />
      </div>

      {/* pcap_dump */}
      {pipeline.pcap_dump && (
        <div className="px-4 py-3">
          <div className="mb-2 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
            <HardDrive className="size-3.5" /> PCAP dump
          </div>
          {pipeline.pcap_dump.enabled ? (
            <div className="grid grid-cols-1 gap-1 text-xs sm:grid-cols-2">
              <KV k="dir" v={pipeline.pcap_dump.dir} mono />
              <KV k="compression" v={pipeline.pcap_dump.compression} />
              {pipeline.pcap_dump.retention && (
                <>
                  <KV
                    k="retention"
                    v={pipeline.pcap_dump.retention.enabled ? "on" : "off"}
                  />
                  <KV
                    k="max age"
                    v={fmtHours(pipeline.pcap_dump.retention.max_age_hours)}
                  />
                  <KV
                    k="max size"
                    v={fmtMiB(pipeline.pcap_dump.retention.max_size_mb)}
                  />
                </>
              )}
              <KV
                k="files deleted"
                v={fmtCounter(metrics["dump_retention_files_deleted"])}
              />
              <KV
                k="bytes deleted"
                v={fmtBytes(metrics["dump_retention_bytes_deleted"])}
              />
              <KV k="dump errors" v={fmtCounter(metrics["dump_errors"])} />
            </div>
          ) : (
            <span className="text-xs italic text-muted-foreground">disabled</span>
          )}
        </div>
      )}
    </div>
  )
}

function SourceRow({ source }: { source: CaptureSource }) {
  if (source.type === "pcap") {
    return (
      <li className="rounded-md border border-border/60 bg-background px-3 py-2 text-xs">
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <span className="rounded bg-primary/10 px-1.5 py-0.5 font-mono text-[10px] uppercase text-primary">
            pcap
          </span>
          <span className="font-mono text-sm">{source.interface}</span>
          {source.source_id && source.source_id !== source.interface && (
            <span className="text-muted-foreground">
              (id <span className="font-mono">{source.source_id}</span>)
            </span>
          )}
        </div>
        <div className="mt-1 grid grid-cols-1 gap-x-3 gap-y-0.5 text-muted-foreground sm:grid-cols-2">
          <KV
            k="BPF"
            v={source.bpf_filter && source.bpf_filter !== "" ? source.bpf_filter : "(none — all TCP)"}
            mono
          />
          <KV k="snaplen" v={`${source.snaplen.toLocaleString()} B`} />
        </div>
      </li>
    )
  }
  if (source.type === "pcap-file") {
    return (
      <li className="rounded-md border border-border/60 bg-background px-3 py-2 text-xs">
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <span className="rounded bg-primary/10 px-1.5 py-0.5 font-mono text-[10px] uppercase text-primary">
            pcap-file
          </span>
          <span className="break-all font-mono">{source.path}</span>
          <span className="text-muted-foreground">
            ({source.realtime ? "realtime replay" : "as-fast-as-possible"})
          </span>
        </div>
      </li>
    )
  }
  return (
    <li className="rounded-md border border-border/60 bg-background px-3 py-2 text-xs">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <span className="rounded bg-primary/10 px-1.5 py-0.5 font-mono text-[10px] uppercase text-primary">
          cloud-probe
        </span>
        <span className="font-mono">{source.endpoint}</span>
        <span className="text-muted-foreground">recv_hwm={source.recv_hwm}</span>
      </div>
    </li>
  )
}

// ============================================================================
// InterfacesCard
// ============================================================================

function InterfacesCard({
  interfaces,
  error,
}: {
  interfaces: CaptureInterface[]
  error: unknown
}) {
  return (
    <div className="rounded-lg border border-border bg-card">
      <div className="flex items-center gap-2 border-b border-border px-4 py-2.5">
        <Network className="size-4 text-muted-foreground" />
        <span className="text-sm font-semibold">Available interfaces</span>
        <span className="ml-auto text-xs text-muted-foreground">
          enumerated by libpcap on this host
        </span>
      </div>
      {error ? (
        <div className="px-4 py-3 text-sm text-destructive">{String(error)}</div>
      ) : interfaces.length === 0 ? (
        <div className="px-4 py-3 text-sm text-muted-foreground">no interfaces visible</div>
      ) : (
        <table className="w-full text-xs">
          <thead className="bg-muted/30 text-muted-foreground">
            <tr>
              <th className="px-4 py-2 text-left font-medium">name</th>
              <th className="px-4 py-2 text-left font-medium">addresses</th>
              <th className="px-4 py-2 text-left font-medium">flags</th>
              <th className="px-4 py-2 text-left font-medium">description</th>
            </tr>
          </thead>
          <tbody>
            {interfaces.map((i) => (
              <tr key={i.name} className="border-t border-border/60">
                <td className="px-4 py-1.5 font-mono">{i.name}</td>
                <td className="px-4 py-1.5 font-mono text-muted-foreground">
                  {i.addresses.length > 0 ? i.addresses.join(", ") : "—"}
                </td>
                <td className="px-4 py-1.5">
                  <FlagRow iface={i} />
                </td>
                <td className="px-4 py-1.5 text-muted-foreground">{i.description ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  )
}

function FlagRow({ iface }: { iface: CaptureInterface }) {
  const flags: { label: string; on: boolean }[] = [
    { label: "up", on: iface.is_up },
    { label: "running", on: iface.is_running },
    { label: "loopback", on: iface.is_loopback },
    { label: "wireless", on: iface.is_wireless },
  ]
  return (
    <div className="flex flex-wrap gap-1">
      {flags
        .filter((f) => f.on)
        .map((f) => (
          <span
            key={f.label}
            className="rounded bg-muted px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide"
          >
            {f.label}
          </span>
        ))}
    </div>
  )
}

// ============================================================================
// Helpers
// ============================================================================

function KV({ k, v, mono = false }: { k: string; v: ReactNode; mono?: boolean }) {
  return (
    <span className="inline-flex gap-1.5">
      <span className="text-muted-foreground">{k}:</span>
      <span className={mono ? "font-mono break-all" : ""}>{v}</span>
    </span>
  )
}

function CountersGrid({
  metrics,
  keys,
}: {
  metrics: Record<string, number>
  keys: string[]
}) {
  return (
    <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs sm:grid-cols-3 md:grid-cols-4">
      {keys.map((k) => {
        const v = metrics[k]
        return (
          <span key={k} className="inline-flex flex-col">
            <span className="text-muted-foreground">{k}</span>
            <span className="font-mono">{fmtCounter(v)}</span>
          </span>
        )
      })}
    </div>
  )
}

function buildMetricIndex(
  pipelines: { name: string; metrics: MetricRecord[] }[],
): Map<string, Record<string, number>> {
  const out = new Map<string, Record<string, number>>()
  for (const p of pipelines) {
    const byName: Record<string, number> = {}
    for (const m of p.metrics) byName[m.name] = m.value
    out.set(p.name, byName)
  }
  return out
}

function fmtCounter(n: number | undefined): string {
  if (n === undefined || n === null || Number.isNaN(n)) return "—"
  return n.toLocaleString()
}

function fmtHours(h: number): string {
  if (h === 0) return "no limit"
  if (h % 24 === 0) return `${h / 24} d`
  return `${h} h`
}

function fmtMiB(mb: number): string {
  if (mb === 0) return "no limit"
  if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GiB`
  return `${mb} MiB`
}

function fmtBytes(n: number | undefined): string {
  if (n === undefined || n === null) return "—"
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(2)} GiB`
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(2)} MiB`
  if (n >= 1 << 10) return `${(n / (1 << 10)).toFixed(2)} KiB`
  return `${n} B`
}
