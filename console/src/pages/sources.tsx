import { useEffect, useState } from "react"
import { Loader2, Radio, HardDrive, Satellite, Info } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatNumber } from "@/lib/format"
import { useSources } from "@/hooks/use-sources"
import { LabPanel } from "@/components/lab/LabPanel"
import type { SourceKind, SourceSnapshot } from "@/types/api"

type Status = "online" | "idle" | "offline" | "pending"

const ONLINE_MS = 60_000
const IDLE_MS = 300_000

function statusOf(asOf: number, s: SourceSnapshot): Status {
  if (s.last_seen_ms === 0) return "pending"
  const age = asOf - s.last_seen_ms
  if (age < ONLINE_MS) return "online"
  if (age < IDLE_MS) return "idle"
  return "offline"
}

function StatusBadge({ status }: { status: Status }) {
  const palette = {
    online: "bg-emerald-500/15 text-emerald-300 border-emerald-500/30",
    idle: "bg-amber-500/15 text-amber-300 border-amber-500/30",
    offline: "bg-rose-500/15 text-rose-300 border-rose-500/30",
    pending: "bg-slate-500/15 text-slate-300 border-slate-500/30",
  }[status]

  const label = {
    online: "ONLINE",
    idle: "IDLE",
    offline: "OFFLINE",
    pending: "PENDING",
  }[status]

  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[10px] font-mono tracking-wider uppercase",
        palette,
      )}
    >
      <span
        className={cn(
          "size-1.5 rounded-full",
          status === "online" && "bg-emerald-400 animate-pulse",
          status === "idle" && "bg-amber-400",
          status === "offline" && "bg-rose-400",
          status === "pending" && "bg-slate-400",
        )}
      />
      {label}
    </span>
  )
}

function relativeTime(asOf: number, lastSeen: number): string {
  if (lastSeen === 0) return "never"
  const age = Math.max(0, asOf - lastSeen)
  const s = Math.floor(age / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ${s % 60}s ago`
  const h = Math.floor(m / 60)
  return `${h}h ${m % 60}m ago`
}

function kindLabel(kind: SourceKind): string {
  switch (kind) {
    case "pcap":
      return "PCAP"
    case "pcap_file":
      return "PCAP FILE"
    case "cloud_probe_receiver":
      return "RECEIVER"
    case "cloud_probe_peer":
      return "PROBE"
  }
}

function KindBadge({ kind }: { kind: SourceKind }) {
  const palette = {
    pcap: "bg-cyan-500/15 text-cyan-300 border-cyan-500/30",
    pcap_file: "bg-violet-500/15 text-violet-300 border-violet-500/30",
    cloud_probe_receiver: "bg-indigo-500/15 text-indigo-300 border-indigo-500/30",
    cloud_probe_peer: "bg-sky-500/15 text-sky-300 border-sky-500/30",
  }[kind]
  return (
    <span
      className={cn(
        "inline-flex items-center rounded border px-1.5 py-0.5 font-mono text-[10px] tracking-wider uppercase",
        palette,
      )}
    >
      {kindLabel(kind)}
    </span>
  )
}

function truncateUuid(uuid: string): string {
  // ZMQ UUID format: 01020304-0506-0708-090a-0b0c0d0e0f10.
  if (uuid.length < 14) return uuid
  return `${uuid.slice(0, 8)}…${uuid.slice(-4)}`
}

function KpiCard({ title, value, subtext }: { title: string; value: string; subtext?: string }) {
  return (
    <LabPanel className="group">
      <div className="flex items-start justify-between mb-2">
        <span className="text-[10px] font-bold tracking-widest uppercase text-muted-foreground/60">
          {title}
        </span>
      </div>
      <div className="text-2xl font-mono tracking-tight text-cyan-300">{value}</div>
      {subtext && (
        <div className="text-[10px] text-muted-foreground/40 mt-1 font-mono">{subtext}</div>
      )}
    </LabPanel>
  )
}

function useTicking(intervalMs = 1000) {
  const [, setTick] = useState(0)
  useEffect(() => {
    const t = setInterval(() => setTick((n) => n + 1), intervalMs)
    return () => clearInterval(t)
  }, [intervalMs])
}

export function SourcesPage() {
  const { data, isLoading, error } = useSources()
  // Update "N s ago" strings every second regardless of poll cadence.
  useTicking(1000)

  if (isLoading) {
    return (
      <div className="flex h-full items-center justify-center bg-background lab-scanline">
        <Loader2 className="size-6 animate-spin text-primary" />
      </div>
    )
  }
  if (error) {
    return (
      <div className="flex h-full items-center justify-center bg-background p-6">
        <LabPanel className="max-w-md">
          <div className="text-rose-400 text-sm font-mono">
            Failed to load sources: {(error as Error).message}
          </div>
        </LabPanel>
      </div>
    )
  }

  if (!data) return null

  // `as_of_ms` from server for monotonic age math. Prefer server time over
  // the browser clock — avoids skew between browser and server
  // misclassifying online/offline.
  const asOf = data.as_of_ms
  const sources = data.sources

  const local = sources.filter((s) => s.kind === "pcap" || s.kind === "pcap_file")
  const receivers = sources.filter((s) => s.kind === "cloud_probe_receiver")
  const peers = sources.filter((s) => s.kind === "cloud_probe_peer")

  const totalSources = sources.length
  const onlineCount = sources.filter((s) => statusOf(asOf, s) === "online").length
  const totalPackets = sources.reduce((acc, s) => acc + s.packets, 0)

  return (
    <div className="flex flex-col gap-6 p-6 min-h-full bg-background lab-scanline overflow-x-hidden">
      {/* KPI row */}
      <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
        <KpiCard title="Total Sources" value={String(totalSources)} subtext="local + remote" />
        <KpiCard
          title="Online"
          value={String(onlineCount)}
          subtext={`of ${totalSources} active in last 60 s`}
        />
        <KpiCard title="Total Packets" value={formatNumber(totalPackets)} subtext="lifetime" />
      </div>

      {/* Local sources */}
      <LabPanel
        title="Local Sources"
        headerExtra={<HardDrive className="size-3 text-cyan-400/60" />}
      >
        {local.length === 0 ? (
          <div className="py-8 text-center text-sm text-muted-foreground/60 font-mono">
            No local capture source configured.
          </div>
        ) : (
          <LocalTable asOf={asOf} rows={local} />
        )}
      </LabPanel>

      {/* Cloud probes */}
      <LabPanel
        title="Cloud Probes"
        headerExtra={
          <div className="flex items-center gap-2">
            <Satellite className="size-3 text-indigo-400/60" />
            <span
              className="inline-flex items-center gap-1 text-[10px] text-muted-foreground/60 font-mono"
              title="Remote IP unavailable via current ZMQ stack; UUID is the canonical identity."
            >
              <Info className="size-3" /> UUID-identified
            </span>
          </div>
        }
      >
        {receivers.length === 0 ? (
          <div className="py-8 text-center text-sm text-muted-foreground/60 font-mono">
            No cloud-probe receiver configured.
          </div>
        ) : (
          <div className="flex flex-col gap-6">
            {receivers.map((r) => {
              const childPeers = peers.filter((p) => p.parent_key === r.key)
              return <ReceiverGroup key={r.key} asOf={asOf} receiver={r} peers={childPeers} />
            })}
          </div>
        )}
      </LabPanel>
    </div>
  )
}

function LocalTable({ asOf, rows }: { asOf: number; rows: SourceSnapshot[] }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="text-left text-[10px] tracking-wider text-muted-foreground/60 font-mono uppercase">
            <th className="py-2 pr-4">Kind</th>
            <th className="py-2 pr-4">Stream ID</th>
            <th className="py-2 pr-4">Endpoint</th>
            <th className="py-2 pr-4 text-right">Packets</th>
            <th className="py-2 pr-4 text-right">Heartbeats</th>
            <th className="py-2 pr-4">Last Seen</th>
            <th className="py-2">Status</th>
          </tr>
        </thead>
        <tbody className="font-mono text-xs">
          {rows.map((s) => {
            const status = statusOf(asOf, s)
            return (
              <tr key={s.key} className="border-t border-white/5">
                <td className="py-2 pr-4">
                  <KindBadge kind={s.kind} />
                </td>
                <td className="py-2 pr-4">{s.key}</td>
                <td className="py-2 pr-4 text-muted-foreground">{s.endpoint}</td>
                <td className="py-2 pr-4 text-right">{formatNumber(s.packets)}</td>
                <td className="py-2 pr-4 text-right">{formatNumber(s.heartbeats)}</td>
                <td className="py-2 pr-4 text-muted-foreground">
                  {relativeTime(asOf, s.last_seen_ms)}
                </td>
                <td className="py-2">
                  <StatusBadge status={status} />
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

function ReceiverGroup({
  asOf,
  receiver,
  peers,
}: {
  asOf: number
  receiver: SourceSnapshot
  peers: SourceSnapshot[]
}) {
  const totalPeerPackets = peers.reduce((a, p) => a + p.packets, 0)
  const status = statusOf(asOf, receiver)

  return (
    <div className="border border-white/5 rounded-lg overflow-hidden">
      <div className="bg-white/2 px-4 py-2 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <Radio className="size-4 text-indigo-400" />
          <KindBadge kind={receiver.kind} />
          <span className="font-mono text-xs text-foreground">{receiver.endpoint}</span>
        </div>
        <div className="flex items-center gap-4 text-[10px] font-mono text-muted-foreground">
          <span>
            {peers.length} peer{peers.length === 1 ? "" : "s"}
          </span>
          <span>·</span>
          <span>{formatNumber(totalPeerPackets)} pkts</span>
          <StatusBadge status={status} />
        </div>
      </div>

      {peers.length === 0 ? (
        <div className="px-4 py-6 text-center text-xs text-muted-foreground/60 font-mono">
          Waiting for first batch. Point a cloud-probe at{" "}
          <span className="text-foreground">{receiver.endpoint}</span>.
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-[10px] tracking-wider text-muted-foreground/60 font-mono uppercase">
                <th className="py-2 pl-4 pr-4">Probe UUID</th>
                <th className="py-2 pr-4 text-right">Packets</th>
                <th className="py-2 pr-4 text-right">Heartbeats</th>
                <th className="py-2 pr-4">First Seen</th>
                <th className="py-2 pr-4">Last Seen</th>
                <th className="py-2 pr-4">Status</th>
              </tr>
            </thead>
            <tbody className="font-mono text-xs">
              {peers.map((p) => {
                const pstatus = statusOf(asOf, p)
                return (
                  <tr key={p.key} className="border-t border-white/5">
                    <td className="py-2 pl-4 pr-4" title={p.key}>
                      <span className="text-sky-300">{truncateUuid(p.key)}</span>
                    </td>
                    <td className="py-2 pr-4 text-right">{formatNumber(p.packets)}</td>
                    <td className="py-2 pr-4 text-right">{formatNumber(p.heartbeats)}</td>
                    <td className="py-2 pr-4 text-muted-foreground">
                      {relativeTime(asOf, p.first_seen_ms)}
                    </td>
                    <td className="py-2 pr-4 text-muted-foreground">
                      {relativeTime(asOf, p.last_seen_ms)}
                    </td>
                    <td className="py-2 pr-4">
                      <StatusBadge status={pstatus} />
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}
