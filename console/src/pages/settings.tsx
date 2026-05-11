import { useRef, useState } from "react"
import type { ReactNode } from "react"
import {
  Loader2,
  RefreshCw,
  Cpu,
  Network,
  HardDrive,
  Pencil,
  Plus,
  Trash2,
  AlertCircle,
  Power,
} from "lucide-react"
import { useQueryClient } from "@tanstack/react-query"
import { useRuntimeConfig } from "@/hooks/use-runtime-config"
import { useInternalMetrics } from "@/hooks/use-internal-metrics"
import { useCaptureInterfaces } from "@/hooks/use-capture-interfaces"
import { useUpdateSources } from "@/hooks/use-update-sources"
import { apiFetch, ApiError } from "@/lib/api"
import type {
  AppConfigShape,
  CaptureInterface,
  CaptureSource,
  MetricRecord,
  PipelineShape,
} from "@/types/api"

/**
 * Settings page — view the capture configuration the running tokenscope
 * process is using, with live per-pipeline counters, and edit the source
 * list. Saving rewrites the on-disk TOML and triggers a self-restart of
 * the tokenscope process; the page polls /api/health until the new
 * process comes back, then refetches all queries.
 */
export function SettingsPage() {
  const queryClient = useQueryClient()
  const config = useRuntimeConfig()
  const metrics = useInternalMetrics()
  const interfaces = useCaptureInterfaces()
  const mutate = useUpdateSources()

  const [editing, setEditing] = useState<string | null>(null)
  const [restartState, setRestartState] = useState<"idle" | "saving" | "restarting">("idle")

  // Snapshot of `loaded_at_ms` when the user triggered the restart, so we
  // can detect when the new process has come up (its `loaded_at_ms` will
  // be larger than this).
  const previousLoadedAtRef = useRef<number | null>(null)

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
  const availableInterfaces = interfaces.data?.interfaces ?? []

  const onSave = async (pipelineName: string, sources: CaptureSource[]) => {
    previousLoadedAtRef.current = config.data?.loaded_at_ms ?? null
    setRestartState("saving")
    try {
      await mutate.mutateAsync({ pipeline_name: pipelineName, sources })
    } catch (e) {
      setRestartState("idle")
      throw e
    }
    setEditing(null)
    setRestartState("restarting")
    // Wait for the new process to come up, then refresh.
    await waitForRestart(previousLoadedAtRef.current ?? 0)
    await queryClient.invalidateQueries()
    setRestartState("idle")
  }

  return (
    <div className="relative flex flex-col gap-4 p-4">
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
        Saving a new source list rewrites{" "}
        <span className="font-mono text-foreground">{config.data.config_path}</span> and
        restarts tokenscope. Pipelines are recreated from scratch — in-flight captures
        will see a brief gap.
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
            interfaces={availableInterfaces}
            isEditing={editing === p.name}
            onEditToggle={() =>
              setEditing((cur) => (cur === p.name ? null : p.name))
            }
            onSave={(sources) => onSave(p.name, sources)}
            saveError={mutate.error}
            disabled={restartState !== "idle"}
          />
        ))
      )}

      {/* ===== Available interfaces ===== */}
      <InterfacesCard
        interfaces={availableInterfaces}
        error={interfaces.error}
      />

      {/* ===== Restart overlay ===== */}
      {restartState !== "idle" && <RestartOverlay state={restartState} />}
    </div>
  )
}

// ============================================================================
// PipelineCard
// ============================================================================

function PipelineCard({
  pipeline,
  metrics,
  interfaces,
  isEditing,
  onEditToggle,
  onSave,
  saveError,
  disabled,
}: {
  pipeline: PipelineShape
  metrics: Record<string, number>
  interfaces: CaptureInterface[]
  isEditing: boolean
  onEditToggle: () => void
  onSave: (sources: CaptureSource[]) => Promise<void>
  saveError: unknown
  disabled: boolean
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
        <button
          onClick={onEditToggle}
          disabled={disabled}
          className="ml-auto inline-flex items-center gap-1.5 rounded-md border border-border bg-background px-2.5 py-1 text-xs font-medium hover:bg-muted disabled:opacity-50"
        >
          <Pencil className="size-3.5" />
          {isEditing ? "Cancel" : "Edit sources"}
        </button>
      </div>

      {/* Sources */}
      <div className="border-b border-border px-4 py-3">
        <div className="mb-2 text-xs font-medium text-muted-foreground">Sources</div>
        {isEditing ? (
          <SourceEditor
            initial={pipeline.sources}
            interfaces={interfaces}
            onCancel={onEditToggle}
            onSave={onSave}
            saveError={saveError}
          />
        ) : pipeline.sources.length === 0 ? (
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
// SourceEditor
// ============================================================================

const DEFAULT_SNAPLEN = 262_144

function SourceEditor({
  initial,
  interfaces,
  onCancel,
  onSave,
  saveError,
}: {
  initial: CaptureSource[]
  interfaces: CaptureInterface[]
  onCancel: () => void
  onSave: (sources: CaptureSource[]) => Promise<void>
  saveError: unknown
}) {
  const [rows, setRows] = useState<CaptureSource[]>(initial.length ? initial : [defaultPcap()])
  const [saving, setSaving] = useState(false)

  const updateRow = (i: number, patch: Partial<CaptureSource>) => {
    setRows((r) =>
      r.map((row, idx) => (idx === i ? ({ ...row, ...patch } as CaptureSource) : row)),
    )
  }
  const removeRow = (i: number) => {
    setRows((r) => r.filter((_, idx) => idx !== i))
  }
  const addPcap = () => setRows((r) => [...r, defaultPcap()])

  const submit = async () => {
    setSaving(true)
    try {
      await onSave(rows)
    } finally {
      setSaving(false)
    }
  }

  // Only pcap rows are editable inline. Other rows (pcap-file, cloud-probe)
  // are shown read-only to preserve them on save.
  return (
    <div className="flex flex-col gap-3">
      <ul className="flex flex-col gap-2">
        {rows.map((s, i) =>
          s.type === "pcap" ? (
            <PcapEditorRow
              key={i}
              source={s}
              interfaces={interfaces}
              onChange={(p) => updateRow(i, p)}
              onRemove={() => removeRow(i)}
              canRemove={rows.length > 1}
            />
          ) : (
            <li
              key={i}
              className="flex items-start gap-2 rounded-md border border-border/60 bg-muted/20 px-3 py-2 text-xs text-muted-foreground"
            >
              <AlertCircle className="mt-0.5 size-3.5 shrink-0" />
              <div className="flex-1">
                <SourceRow source={s} />
                <div className="mt-1 italic">
                  Non-pcap sources stay as-is; edit via the TOML file directly.
                </div>
              </div>
            </li>
          ),
        )}
      </ul>

      <div className="flex flex-wrap items-center gap-2">
        <button
          onClick={addPcap}
          disabled={saving}
          className="inline-flex items-center gap-1.5 rounded-md border border-border bg-background px-2.5 py-1 text-xs font-medium hover:bg-muted disabled:opacity-50"
        >
          <Plus className="size-3.5" /> Add pcap source
        </button>
        <div className="flex-1" />
        <button
          onClick={onCancel}
          disabled={saving}
          className="rounded-md border border-border bg-background px-3 py-1 text-xs font-medium hover:bg-muted disabled:opacity-50"
        >
          Cancel
        </button>
        <button
          onClick={submit}
          disabled={saving || rows.filter((r) => r.type === "pcap").length === 0}
          className="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-1 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
        >
          {saving && <Loader2 className="size-3.5 animate-spin" />}
          Save &amp; restart
        </button>
      </div>

      {saveError ? (
        <div className="rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-xs text-destructive">
          <div className="flex items-start gap-1.5">
            <AlertCircle className="mt-0.5 size-3.5 shrink-0" />
            <div>{errorMessage(saveError)}</div>
          </div>
        </div>
      ) : null}
    </div>
  )
}

function PcapEditorRow({
  source,
  interfaces,
  onChange,
  onRemove,
  canRemove,
}: {
  source: Extract<CaptureSource, { type: "pcap" }>
  interfaces: CaptureInterface[]
  onChange: (patch: Partial<Extract<CaptureSource, { type: "pcap" }>>) => void
  onRemove: () => void
  canRemove: boolean
}) {
  return (
    <li className="rounded-md border border-border/60 bg-background px-3 py-2">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <span className="rounded bg-primary/10 px-1.5 py-0.5 font-mono text-[10px] uppercase text-primary">
          pcap
        </span>
        <label className="inline-flex items-center gap-1.5">
          <span className="text-muted-foreground">interface</span>
          <select
            value={source.interface}
            onChange={(e) => onChange({ interface: e.target.value })}
            className="rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
          >
            {/* Make sure the current interface is selectable even if it
                isn't in `interfaces` (e.g., enumeration failed). */}
            {!interfaces.some((i) => i.name === source.interface) && (
              <option value={source.interface}>{source.interface} (current)</option>
            )}
            {interfaces.map((i) => (
              <option key={i.name} value={i.name}>
                {i.name}
                {i.addresses.length > 0 ? `  ·  ${i.addresses[0]}` : ""}
              </option>
            ))}
          </select>
        </label>
        <label className="inline-flex items-center gap-1.5">
          <span className="text-muted-foreground">snaplen</span>
          <input
            type="number"
            min={64}
            max={1 << 20}
            value={source.snaplen}
            onChange={(e) =>
              onChange({ snaplen: Number(e.target.value) || DEFAULT_SNAPLEN })
            }
            className="w-24 rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
          />
        </label>
        <div className="flex-1" />
        {canRemove && (
          <button
            onClick={onRemove}
            className="rounded-md p-1 text-muted-foreground hover:bg-muted hover:text-destructive"
            title="Remove this source"
          >
            <Trash2 className="size-3.5" />
          </button>
        )}
      </div>
      <label className="mt-2 flex items-center gap-2 text-xs">
        <span className="text-muted-foreground">BPF</span>
        <input
          type="text"
          value={source.bpf_filter ?? ""}
          placeholder="(empty = all TCP)"
          onChange={(e) =>
            onChange({ bpf_filter: e.target.value === "" ? null : e.target.value })
          }
          className="flex-1 rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
        />
      </label>
    </li>
  )
}

// ============================================================================
// InterfacesCard (read-only)
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
        <div className="px-4 py-3 text-sm text-destructive">{errorMessage(error)}</div>
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
// Restart overlay + helpers
// ============================================================================

function RestartOverlay({ state }: { state: "saving" | "restarting" }) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-background/80 backdrop-blur-sm">
      <div className="flex flex-col items-center gap-3 rounded-lg border border-border bg-card px-8 py-6 shadow-lg">
        {state === "saving" ? (
          <>
            <Loader2 className="size-8 animate-spin text-primary" />
            <div className="text-sm font-medium">Saving configuration…</div>
            <div className="text-xs text-muted-foreground">
              Validating and writing TOML
            </div>
          </>
        ) : (
          <>
            <Power className="size-8 animate-pulse text-primary" />
            <div className="text-sm font-medium">Restarting tokenscope…</div>
            <div className="text-xs text-muted-foreground">
              Capture pipeline is being recreated. This usually takes a few seconds.
            </div>
          </>
        )}
      </div>
    </div>
  )
}

/**
 * Poll /api/health until the running process is the *new* one (its
 * loaded_at_ms is greater than what we had before the save), capped at
 * 60 attempts × 1s = 60s in case execv() failed and old process keeps
 * serving.
 */
async function waitForRestart(prevLoadedAtMs: number): Promise<void> {
  // Give the server its scheduled 500ms restart delay plus a touch more
  // before the first probe — earlier probes will all succeed against the
  // old process and add noise.
  await new Promise((r) => setTimeout(r, 1200))

  for (let i = 0; i < 60; i++) {
    try {
      const rt = await apiFetch<{ loaded_at_ms: number }>("/api/runtime-config")
      if (rt.loaded_at_ms > prevLoadedAtMs) return
    } catch {
      // socket-closed during exec is expected — swallow and retry.
    }
    await new Promise((r) => setTimeout(r, 1000))
  }
  // Fall through silently. The page-level invalidate will still refresh
  // whatever state we can reach.
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

function KV({ k, v, mono = false }: { k: string; v: ReactNode; mono?: boolean }) {
  return (
    <span className="inline-flex gap-1.5">
      <span className="text-muted-foreground">{k}:</span>
      <span className={mono ? "font-mono break-all" : ""}>{v}</span>
    </span>
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

function defaultPcap(): CaptureSource {
  return {
    type: "pcap",
    interface: "any",
    bpf_filter: null,
    snaplen: DEFAULT_SNAPLEN,
    source_id: null,
  }
}

function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message
  if (e instanceof Error) return e.message
  return String(e ?? "unknown error")
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

