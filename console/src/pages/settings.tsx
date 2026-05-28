import { useRef, useState } from "react"
import type { ReactNode } from "react"
import {
  Loader2,
  RefreshCw,
  Cpu,
  HardDrive,
  Pencil,
  Plus,
  AlertCircle,
  Power,
  Info,
  ChevronDown,
  ChevronRight,
  Network,
} from "lucide-react"
import { useQueryClient } from "@tanstack/react-query"
import { useRuntimeConfig } from "@/hooks/use-runtime-config"
import { useInternalMetrics } from "@/hooks/use-internal-metrics"
import { useCaptureInterfaces } from "@/hooks/use-capture-interfaces"
import { useUpdateSources } from "@/hooks/use-update-sources"
import { apiFetch, ApiError } from "@/lib/api"
import { groupInterfaces } from "@/lib/interface-groups"
import type {
  AppConfigShape,
  CaptureInterface,
  CaptureSource,
  MetricRecord,
  PipelineShape,
} from "@/types/api"
import { SourceEditorRow, defaultFor } from "@/components/settings/source-editor"

/**
 * Settings page — friendly view + edit of the capture configuration the
 * running heron process is using. Source list is structured by
 * source type (live NIC / ZMQ receiver / PCAP replay); editor hides BPF
 * behind a ports+hosts UI for the common case. Saving rewrites the on-
 * disk TOML and self-restarts; the page polls runtime-config until the
 * new process is up.
 */
export function SettingsPage() {
  const queryClient = useQueryClient()
  const config = useRuntimeConfig()
  const metrics = useInternalMetrics()
  const interfaces = useCaptureInterfaces()
  const mutate = useUpdateSources()

  const [editing, setEditing] = useState<string | null>(null)
  const [restartState, setRestartState] = useState<"idle" | "saving" | "restarting">("idle")
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
    await waitForRestart(previousLoadedAtRef.current ?? 0)
    await queryClient.invalidateQueries()
    setRestartState("idle")
  }

  return (
    <div className="relative flex flex-col gap-4 p-4">
      <PageHeader
        version={config.data.version}
        configPath={config.data.config_path}
        isFetching={config.isFetching}
        onRefresh={() => {
          config.refetch()
          metrics.refetch()
          interfaces.refetch()
        }}
      />

      {/* Pipelines */}
      {pipelines.length === 0 ? (
        <EmptyState>
          No pipelines configured — heron is running in CLI mode (started with
          <code className="mx-1 rounded bg-muted px-1 font-mono">--pcap-file</code> or
          <code className="mx-1 rounded bg-muted px-1 font-mono">-i</code>). Restart
          without those flags to edit pipelines here.
        </EmptyState>
      ) : (
        pipelines.map((p) => (
          <PipelineCard
            key={p.name}
            pipeline={p}
            metrics={metricsByPipeline.get(p.name) ?? {}}
            interfaces={availableInterfaces}
            isEditing={editing === p.name}
            onEditToggle={() => setEditing((cur) => (cur === p.name ? null : p.name))}
            onSave={(sources) => onSave(p.name, sources)}
            saveError={mutate.error}
            disabled={restartState !== "idle"}
          />
        ))
      )}

      <InterfaceHelpExpander interfaces={availableInterfaces} />

      {restartState !== "idle" && <RestartOverlay state={restartState} />}
    </div>
  )
}

// ============================================================================
// Header
// ============================================================================

function PageHeader({
  version,
  configPath,
  isFetching,
  onRefresh,
}: {
  version: string
  configPath: string
  isFetching: boolean
  onRefresh: () => void
}) {
  return (
    <div className="rounded-lg border border-border bg-card">
      <div className="flex flex-wrap items-center gap-3 px-4 py-2.5">
        <span className="text-sm font-semibold">Settings</span>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span>
            version <span className="font-mono text-foreground">{version}</span>
          </span>
          <span className="break-all">
            config <span className="font-mono text-foreground">{configPath}</span>
          </span>
        </div>
        <button
          onClick={onRefresh}
          className="ml-auto inline-flex items-center gap-1.5 rounded-md border border-border bg-background px-2.5 py-1 text-xs font-medium hover:bg-muted"
        >
          <RefreshCw className={isFetching ? "size-3.5 animate-spin" : "size-3.5"} />
          Refresh
        </button>
      </div>
      <div className="flex items-start gap-2 border-t border-border bg-muted/30 px-4 py-2 text-xs text-muted-foreground">
        <Info className="mt-0.5 size-3.5 shrink-0" />
        <p>
          Capture sources tell heron where packets come from — a live network
          interface, a remote ZMQ stream from a probe, or a PCAP file. Saving here
          rewrites the config file and restarts the process; capture pauses for
          about 2–3 seconds while the new pipeline comes up.
        </p>
      </div>
    </div>
  )
}

function EmptyState({ children }: { children: ReactNode }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4 text-sm text-muted-foreground">
      {children}
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
        <span className="text-sm font-semibold">Pipeline · {pipeline.name}</span>
        {pipeline.dispatcher_count !== undefined && (
          <span className="ml-2 text-xs text-muted-foreground">
            {pipeline.dispatcher_count} dispatcher · {pipeline.flow_shard_count ?? "?"} flow
            shards
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

      {/* Sources — grouped by type */}
      <div className="border-b border-border px-4 py-3">
        <div className="mb-3 flex items-center gap-2 text-xs font-medium text-muted-foreground">
          <span>Capture sources</span>
          <span>·</span>
          <span>{pipeline.sources.length} configured</span>
        </div>
        {isEditing ? (
          <SourceEditor
            initial={pipeline.sources}
            interfaces={interfaces}
            onCancel={onEditToggle}
            onSave={onSave}
            saveError={saveError}
          />
        ) : (
          <SourcesByType
            sources={pipeline.sources}
            isEditing={false}
            interfaces={interfaces}
            onChangeAt={() => {}}
            onRemoveAt={() => {}}
            onAddOfType={() => {}}
          />
        )}
      </div>

      {/* Live counters */}
      <div className="border-b border-border px-4 py-3">
        <div className="mb-2 text-xs font-medium text-muted-foreground">
          Activity (live)
        </div>
        <CountersGrid metrics={metrics} sources={pipeline.sources} />
      </div>

      {/* pcap_dump */}
      {pipeline.pcap_dump && (
        <PcapDumpSection
          dump={pipeline.pcap_dump}
          retentionFilesDeleted={metrics["dump_retention_files_deleted"]}
          retentionBytesDeleted={metrics["dump_retention_bytes_deleted"]}
          dumpErrors={metrics["dump_errors"]}
        />
      )}
    </div>
  )
}

// ============================================================================
// Type metadata — shared between view and edit panels
// ============================================================================

type SourceType = CaptureSource["type"]

const TYPE_META: Record<
  SourceType,
  { icon: string; title: string; addLabel: string; emptyHint: string }
> = {
  pcap: {
    icon: "📡",
    title: "Live captures",
    addLabel: "Add live capture",
    emptyHint:
      "(no live captures — packets are not being read from any local NIC)",
  },
  "cloud-probe": {
    icon: "🔌",
    title: "ZMQ receivers",
    addLabel: "Add ZMQ receiver",
    emptyHint:
      "(none — receive packets streamed in from remote heron probes)",
  },
  "pcap-file": {
    icon: "📂",
    title: "PCAP replay",
    addLabel: "Add file replay",
    emptyHint: "(none — replay packets from a saved .pcap file, for dev / forensic)",
  },
}

// ============================================================================
// SourcesByType — three per-type sections, used by both view and edit modes
// ============================================================================

function SourcesByType({
  sources,
  isEditing,
  interfaces,
  onChangeAt,
  onRemoveAt,
  onAddOfType,
}: {
  sources: CaptureSource[]
  isEditing: boolean
  interfaces: CaptureInterface[]
  onChangeAt: (i: number, next: CaptureSource) => void
  onRemoveAt: (i: number) => void
  onAddOfType: (type: SourceType) => void
}) {
  // Group original indices by type so removal/update can address the
  // original array without reordering.
  const groups: Record<SourceType, number[]> = {
    pcap: [],
    "cloud-probe": [],
    "pcap-file": [],
  }
  sources.forEach((s, i) => groups[s.type].push(i))
  const order: SourceType[] = ["pcap", "cloud-probe", "pcap-file"]
  return (
    <div className="flex flex-col gap-3">
      {order.map((type) => (
        <SourceTypeSection
          key={type}
          type={type}
          indices={groups[type]}
          sources={sources}
          isEditing={isEditing}
          interfaces={interfaces}
          onChangeAt={onChangeAt}
          onRemoveAt={onRemoveAt}
          onAdd={() => onAddOfType(type)}
        />
      ))}
    </div>
  )
}

function SourceTypeSection({
  type,
  indices,
  sources,
  isEditing,
  interfaces,
  onChangeAt,
  onRemoveAt,
  onAdd,
}: {
  type: SourceType
  indices: number[]
  sources: CaptureSource[]
  isEditing: boolean
  interfaces: CaptureInterface[]
  onChangeAt: (i: number, next: CaptureSource) => void
  onRemoveAt: (i: number) => void
  onAdd: () => void
}) {
  const meta = TYPE_META[type]
  const empty = indices.length === 0
  return (
    <div className="rounded-md border border-border/60">
      <div className="flex items-center gap-2 border-b border-border/60 bg-muted/30 px-3 py-1.5 text-xs">
        <span aria-hidden>{meta.icon}</span>
        <span className="font-semibold">{meta.title}</span>
        <span className="text-muted-foreground">· {indices.length}</span>
        <div className="flex-1" />
        {isEditing && (
          <button
            onClick={onAdd}
            className="inline-flex items-center gap-1 rounded-md border border-border bg-background px-2 py-0.5 hover:bg-muted"
          >
            <Plus className="size-3" /> {meta.addLabel}
          </button>
        )}
      </div>
      <div className="px-3 py-2">
        {empty ? (
          <div className="py-1 text-xs italic text-muted-foreground">{meta.emptyHint}</div>
        ) : (
          <ul className="flex flex-col gap-2">
            {indices.map((i) =>
              isEditing ? (
                <SourceEditorRow
                  key={i}
                  source={sources[i]}
                  interfaces={interfaces}
                  onChange={(next) => onChangeAt(i, next)}
                  onRemove={() => onRemoveAt(i)}
                />
              ) : (
                <SourceSummary key={i} source={sources[i]} />
              ),
            )}
          </ul>
        )}
      </div>
    </div>
  )
}

// ============================================================================
// SourceSummary — read-only one-liner per source (used in view mode)
// ============================================================================

function SourceSummary({ source }: { source: CaptureSource }) {
  if (source.type === "pcap") {
    return (
      <li className="rounded-md border border-border/60 bg-background px-3 py-2 text-xs">
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <span className="font-mono text-sm">
            interface <span className="font-semibold">{source.interface}</span>
          </span>
        </div>
        <div className="mt-1 text-muted-foreground">
          {describeBpf(source.bpf_filter)}
          <span className="ml-3">snaplen {source.snaplen.toLocaleString()} B</span>
        </div>
      </li>
    )
  }
  if (source.type === "cloud-probe") {
    return (
      <li className="rounded-md border border-border/60 bg-background px-3 py-2 text-xs">
        <div className="font-mono text-sm">listen {source.endpoint}</div>
        <div className="mt-1 text-muted-foreground">
          receive queue depth {source.recv_hwm}
        </div>
      </li>
    )
  }
  return (
    <li className="rounded-md border border-border/60 bg-background px-3 py-2 text-xs">
      <div className="break-all font-mono text-sm">{source.path}</div>
      <div className="mt-1 text-muted-foreground">
        {source.realtime ? "replay at original speed" : "replay as fast as possible"}
      </div>
    </li>
  )
}

function describeBpf(bpf: string | null): string {
  if (!bpf || bpf.trim() === "") return "capturing all TCP traffic"
  // Heuristic plain-English description for the common cases. Anything
  // exotic falls back to the raw filter expression.
  const ports = Array.from(bpf.matchAll(/(?:tcp\s+)?port\s+(\d{1,5})/gi)).map((m) => m[1])
  const hosts = Array.from(bpf.matchAll(/host\s+(\S+)/gi)).map((m) => m[1])
  const parts: string[] = []
  if (ports.length > 0) parts.push(`port${ports.length > 1 ? "s" : ""} ${ports.join(", ")}`)
  if (hosts.length > 0) parts.push(`host${hosts.length > 1 ? "s" : ""} ${hosts.join(", ")}`)
  if (parts.length === 0) {
    return `filter: ${bpf}`
  }
  return `${parts.join(" · ")}`
}

// ============================================================================
// SourceEditor — wraps SourcesByType with local row state + Save/Cancel
// ============================================================================

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
  const [rows, setRows] = useState<CaptureSource[]>(initial)
  const [saving, setSaving] = useState(false)
  const [confirming, setConfirming] = useState(false)

  const updateRow = (i: number, next: CaptureSource) => {
    setRows((r) => r.map((row, idx) => (idx === i ? next : row)))
  }
  const removeRow = (i: number) => {
    setRows((r) => r.filter((_, idx) => idx !== i))
    setConfirming(false)
  }
  const addOfType = (type: SourceType) => {
    setRows((r) => [...r, defaultFor(type)])
    setConfirming(false)
  }

  const submit = async () => {
    setSaving(true)
    try {
      await onSave(rows)
    } finally {
      setSaving(false)
    }
  }

  const canSave = rows.length > 0

  return (
    <div className="flex flex-col gap-3">
      <SourcesByType
        sources={rows}
        isEditing
        interfaces={interfaces}
        onChangeAt={updateRow}
        onRemoveAt={removeRow}
        onAddOfType={addOfType}
      />

      <div className="flex flex-wrap items-center gap-2">
        <div className="flex-1" />
        {!confirming ? (
          <>
            <button
              onClick={onCancel}
              disabled={saving}
              className="rounded-md border border-border bg-background px-3 py-1 text-xs font-medium hover:bg-muted disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              onClick={() => setConfirming(true)}
              disabled={saving || !canSave}
              className="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-1 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
              title={!canSave ? "Add at least one source before saving" : undefined}
            >
              Save…
            </button>
          </>
        ) : (
          <div className="flex flex-wrap items-center gap-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-1.5 text-xs">
            <AlertCircle className="size-3.5 shrink-0 text-amber-600 dark:text-amber-400" />
            <span>
              Capture will pause for ~2–3 s while heron restarts. Continue?
            </span>
            <button
              onClick={() => setConfirming(false)}
              disabled={saving}
              className="rounded-md border border-border bg-background px-2 py-0.5 hover:bg-muted disabled:opacity-50"
            >
              No
            </button>
            <button
              onClick={submit}
              disabled={saving}
              className="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-0.5 font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
            >
              {saving && <Loader2 className="size-3.5 animate-spin" />}
              Yes, save & restart
            </button>
          </div>
        )}
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

// ============================================================================
// Counters — split by source-type so the live-capture and ZMQ-receiver
// metrics don't sit jumbled in one grid. Sections with no matching source
// in the pipeline are omitted entirely (a pipeline with only ZMQ won't
// render dead-zero packet counters).
// ============================================================================

interface CounterDef {
  key: string
  label: string
  hint?: string
}

const LIVE_COUNTERS: CounterDef[] = [
  { key: "pkts_received", label: "Packets captured" },
  {
    key: "pkts_dropped_kernel",
    label: "Dropped — kernel ring full",
    hint: "Capture couldn't keep up; consider a tighter filter or larger snaplen ring.",
  },
  {
    key: "pkts_truncated",
    label: "Truncated to snaplen",
    hint: "Packet was larger than snaplen; bodies past the cutoff are missing.",
  },
  { key: "read_errors", label: "Read errors" },
]

const ZMQ_COUNTERS: CounterDef[] = [
  { key: "batches_received", label: "Batches received" },
  {
    key: "batches_dropped_zmq",
    label: "Batches dropped",
    hint: "ZMQ HWM exceeded; downstream stages are saturated.",
  },
]

function CountersGrid({
  metrics,
  sources,
}: {
  metrics: Record<string, number>
  sources: CaptureSource[]
}) {
  const hasLive = sources.some((s) => s.type === "pcap" || s.type === "pcap-file")
  const hasZmq = sources.some((s) => s.type === "cloud-probe")
  if (!hasLive && !hasZmq) {
    return (
      <div className="text-xs italic text-muted-foreground">
        no sources configured — nothing to count
      </div>
    )
  }
  return (
    <div className="flex flex-col gap-3">
      {hasLive && (
        <CountersSubsection title="Live capture" counters={LIVE_COUNTERS} metrics={metrics} />
      )}
      {hasZmq && (
        <CountersSubsection title="ZMQ receiver" counters={ZMQ_COUNTERS} metrics={metrics} />
      )}
    </div>
  )
}

function CountersSubsection({
  title,
  counters,
  metrics,
}: {
  title: string
  counters: CounterDef[]
  metrics: Record<string, number>
}) {
  return (
    <div>
      <div className="mb-1 text-[11px] uppercase tracking-wide text-muted-foreground">
        {title}
      </div>
      <div className="grid grid-cols-2 gap-x-4 gap-y-2 text-xs sm:grid-cols-3 md:grid-cols-4">
        {counters.map((c) => (
          <div key={c.key} className="flex flex-col">
            <span className="text-muted-foreground" title={c.hint}>
              {c.label}
              {c.hint && <span className="ml-1 text-[10px]">ⓘ</span>}
            </span>
            <span className="font-mono">{fmtCounter(metrics[c.key])}</span>
          </div>
        ))}
      </div>
    </div>
  )
}

// ============================================================================
// PcapDumpSection
// ============================================================================

function PcapDumpSection({
  dump,
  retentionFilesDeleted,
  retentionBytesDeleted,
  dumpErrors,
}: {
  dump: NonNullable<PipelineShape["pcap_dump"]>
  retentionFilesDeleted: number | undefined
  retentionBytesDeleted: number | undefined
  dumpErrors: number | undefined
}) {
  return (
    <div className="px-4 py-3">
      <div className="mb-2 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
        <HardDrive className="size-3.5" /> PCAP dump
      </div>
      {dump.enabled ? (
        <div className="grid grid-cols-1 gap-1 text-xs sm:grid-cols-2">
          <KV k="Directory" v={dump.dir} mono />
          <KV k="Compression" v={dump.compression} />
          {dump.retention && (
            <>
              <KV k="Retention" v={dump.retention.enabled ? "on" : "off"} />
              <KV k="Max age" v={fmtHours(dump.retention.max_age_hours)} />
              <KV k="Max size" v={fmtMiB(dump.retention.max_size_mb)} />
            </>
          )}
          <KV k="Files reclaimed" v={fmtCounter(retentionFilesDeleted)} />
          <KV k="Bytes reclaimed" v={fmtBytes(retentionBytesDeleted)} />
          <KV k="Write errors" v={fmtCounter(dumpErrors)} />
        </div>
      ) : (
        <span className="text-xs italic text-muted-foreground">disabled</span>
      )}
    </div>
  )
}

// ============================================================================
// InterfaceHelpExpander — replaces the dumping all-interfaces table
// ============================================================================

function InterfaceHelpExpander({ interfaces }: { interfaces: CaptureInterface[] }) {
  const [open, setOpen] = useState(false)
  if (interfaces.length === 0) return null
  const groups = groupInterfaces(interfaces)
  return (
    <div className="rounded-lg border border-border bg-card">
      <button
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-4 py-2.5 text-left"
      >
        {open ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
        <Network className="size-4 text-muted-foreground" />
        <span className="text-sm font-semibold">Help me pick an interface</span>
        <span className="ml-auto text-xs text-muted-foreground">
          {groups.recommended.length} recommended · {groups.virtual.length} virtual
        </span>
      </button>
      {open && (
        <div className="border-t border-border px-4 py-3 text-xs">
          <InterfaceTable interfaces={groups.recommended} title="Recommended" />
          {groups.virtual.length > 0 && (
            <details className="mt-3">
              <summary className="cursor-pointer text-muted-foreground hover:text-foreground">
                Virtual interfaces ({groups.virtual.length}) — container veths,
                libvirt taps, etc.
              </summary>
              <div className="mt-2">
                <InterfaceTable interfaces={groups.virtual} />
              </div>
            </details>
          )}
        </div>
      )}
    </div>
  )
}

function InterfaceTable({
  interfaces,
  title,
}: {
  interfaces: CaptureInterface[]
  title?: string
}) {
  return (
    <div>
      {title && <div className="mb-1 font-medium">{title}</div>}
      <table className="w-full">
        <thead className="text-muted-foreground">
          <tr>
            <th className="py-1 text-left font-medium">name</th>
            <th className="py-1 text-left font-medium">addresses</th>
            <th className="py-1 text-left font-medium">flags</th>
          </tr>
        </thead>
        <tbody>
          {interfaces.map((i) => (
            <tr key={i.name} className="border-t border-border/60">
              <td className="py-1 font-mono">{i.name}</td>
              <td className="py-1 font-mono text-muted-foreground">
                {i.addresses.length > 0 ? i.addresses.join(", ") : "—"}
              </td>
              <td className="py-1">
                <FlagRow iface={i} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
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
// Restart overlay + polling
// ============================================================================

function RestartOverlay({ state }: { state: "saving" | "restarting" }) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-background/80 backdrop-blur-sm">
      <div className="flex flex-col items-center gap-3 rounded-lg border border-border bg-card px-8 py-6 shadow-lg">
        {state === "saving" ? (
          <>
            <Loader2 className="size-8 animate-spin text-primary" />
            <div className="text-sm font-medium">Saving configuration…</div>
            <div className="text-xs text-muted-foreground">Validating and writing TOML</div>
          </>
        ) : (
          <>
            <Power className="size-8 animate-pulse text-primary" />
            <div className="text-sm font-medium">Restarting heron…</div>
            <div className="text-xs text-muted-foreground">
              Capture pipeline is being recreated. This usually takes a few seconds.
            </div>
          </>
        )}
      </div>
    </div>
  )
}

async function waitForRestart(prevLoadedAtMs: number): Promise<void> {
  await new Promise((r) => setTimeout(r, 1200))
  for (let i = 0; i < 60; i++) {
    try {
      const rt = await apiFetch<{ loaded_at_ms: number }>("/api/runtime-config")
      if (rt.loaded_at_ms > prevLoadedAtMs) return
    } catch {
      // expected during exec
    }
    await new Promise((r) => setTimeout(r, 1000))
  }
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
