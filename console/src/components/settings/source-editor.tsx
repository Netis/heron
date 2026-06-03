import { useMemo, useState } from "react"
import { AlertCircle, ChevronDown, ChevronRight, Trash2 } from "lucide-react"
import type { CaptureInterface, CaptureSource } from "@/types/api"
import { groupInterfaces } from "@/lib/interface-groups"
import { isValidHost, isValidPort, parseBpf, synthBpf } from "@/lib/bpf"
import { ChipInput } from "./chip-input"

const DEFAULT_SNAPLEN = 262_144
const DEFAULT_CLOUD_PROBE_ENDPOINT = "tcp://0.0.0.0:5555"
const DEFAULT_CLOUD_PROBE_HWM = 1000

/**
 * Default BPF for a newly-added live-capture source: the common ports
 * that LLM serving stacks listen on, so out-of-the-box capture sees
 * traffic without the user having to know BPF syntax. Users can edit
 * via the structured ports/hosts panel or switch to raw BPF.
 *
 *   1234   LM Studio
 *   4000   LiteLLM proxy default
 *   4200   LiteLLM alt (in active use here)
 *   8000   vLLM default
 *   8001   vLLM alt / multi-instance
 *   8080   common HTTP backend (vLLM, OpenAI-compatible servers)
 *   9000   generic alt; SGLang served on this in our internal setup
 *   11434  Ollama default
 *   30000  SGLang default
 */
const DEFAULT_LLM_PORTS = [1234, 4000, 4200, 8000, 8001, 8080, 9000, 11434, 30000]
const DEFAULT_LLM_PORTS_BPF = DEFAULT_LLM_PORTS.map((p) => `tcp port ${p}`).join(" or ")

// ============================================================================
// SourceEditorRow — dispatches by type; the type itself is fixed (no
// in-row switcher) because the Settings page now organises sources into
// per-type sections.
// ============================================================================

export function SourceEditorRow({
  source,
  interfaces,
  onChange,
  onRemove,
}: {
  source: CaptureSource
  interfaces: CaptureInterface[]
  onChange: (next: CaptureSource) => void
  onRemove: () => void
}) {
  return (
    <li className="rounded-md border border-border/60 bg-background p-3">
      <div className="mb-2 flex items-center gap-2 text-xs">
        <span className="text-muted-foreground">{rowHeading(source.type)}</span>
        <div className="flex-1" />
        <button
          onClick={onRemove}
          className="rounded-md p-1 text-muted-foreground hover:bg-muted hover:text-destructive"
          title="Remove this source"
        >
          <Trash2 className="size-3.5" />
        </button>
      </div>
      {source.type === "pcap" && (
        <PcapForm source={source} interfaces={interfaces} onChange={onChange} />
      )}
      {source.type === "cloud-probe" && (
        <CloudProbeForm source={source} onChange={onChange} />
      )}
      {source.type === "pcap-file" && (
        <PcapFileForm source={source} onChange={onChange} />
      )}
    </li>
  )
}

function rowHeading(type: CaptureSource["type"]): string {
  switch (type) {
    case "pcap":
      return "Live capture from local interface"
    case "cloud-probe":
      return "ZMQ receiver for remote probe stream"
    case "pcap-file":
      return "PCAP file replay"
  }
}

export function defaultFor(type: CaptureSource["type"]): CaptureSource {
  if (type === "pcap") {
    return {
      type: "pcap",
      interface: "any",
      bpf_filter: DEFAULT_LLM_PORTS_BPF,
      snaplen: DEFAULT_SNAPLEN,
      source_id: null,
    }
  }
  if (type === "cloud-probe") {
    return {
      type: "cloud-probe",
      endpoint: DEFAULT_CLOUD_PROBE_ENDPOINT,
      recv_hwm: DEFAULT_CLOUD_PROBE_HWM,
    }
  }
  return {
    type: "pcap-file",
    path: "",
    realtime: false,
    source_id: null,
    loop_count: 1,
    loop_secs: 0,
    rate_pps: 0,
  }
}

// ============================================================================
// PcapForm — friendly structured editor with raw-BPF fallback
// ============================================================================

function PcapForm({
  source,
  interfaces,
  onChange,
}: {
  source: Extract<CaptureSource, { type: "pcap" }>
  interfaces: CaptureInterface[]
  onChange: (next: CaptureSource) => void
}) {
  // Decide initial mode: structured if BPF parses cleanly, else raw.
  const parsed = useMemo(() => parseBpf(source.bpf_filter), [source.bpf_filter])
  const [mode, setMode] = useState<"structured" | "raw">(parsed ? "structured" : "raw")
  const [advancedOpen, setAdvancedOpen] = useState(false)

  const groups = useMemo(() => groupInterfaces(interfaces), [interfaces])
  const currentNotInList =
    !interfaces.some((i) => i.name === source.interface) && source.interface !== ""

  // When in structured mode and the user types invalid input we still
  // update bpf_filter to the synth output; the parent server-side
  // validator will reject. We show inline validation hints here too.
  const updateStructured = (ports: number[], hosts: string[]) => {
    const next = synthBpf({ ports, hosts })
    onChange({ ...source, bpf_filter: next === "" ? null : next })
  }

  return (
    <div className="flex flex-col gap-3">
      {/* Interface */}
      <Field
        label="Network interface"
        hint="Where on this host to capture from. 'any' covers every interface, including loopback."
      >
        <select
          value={source.interface}
          onChange={(e) => onChange({ ...source, interface: e.target.value })}
          className="w-full rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
        >
          {currentNotInList && (
            <option value={source.interface}>{source.interface} (current — not in list)</option>
          )}
          <optgroup label="Recommended">
            {groups.recommended.map((i) => (
              <option key={i.name} value={i.name}>
                {formatInterfaceOption(i)}
              </option>
            ))}
          </optgroup>
          {groups.virtual.length > 0 && (
            <optgroup label={`Virtual (${groups.virtual.length})`}>
              {groups.virtual.map((i) => (
                <option key={i.name} value={i.name}>
                  {formatInterfaceOption(i)}
                </option>
              ))}
            </optgroup>
          )}
        </select>
      </Field>

      {/* What to capture */}
      <div className="rounded-md border border-border/60 bg-muted/20 p-3">
        <div className="mb-2 flex items-center justify-between text-xs">
          <span className="font-medium">What to capture</span>
          <button
            type="button"
            onClick={() => setMode((m) => (m === "structured" ? "raw" : "structured"))}
            className="text-muted-foreground hover:text-foreground"
          >
            {mode === "structured" ? "Switch to raw BPF" : "Switch to ports/hosts"}
          </button>
        </div>

        {mode === "structured" ? (
          <StructuredFilter
            ports={parsed?.ports ?? []}
            hosts={parsed?.hosts ?? []}
            onChange={updateStructured}
          />
        ) : (
          <RawBpfInput
            value={source.bpf_filter ?? ""}
            onChange={(v) => onChange({ ...source, bpf_filter: v === "" ? null : v })}
            structuredAvailable={parsed !== null}
            onBackToStructured={() => setMode("structured")}
          />
        )}
      </div>

      {/* Advanced */}
      <Disclosure
        open={advancedOpen}
        onToggle={() => setAdvancedOpen((v) => !v)}
        label="Advanced"
      >
        <Field
          label="Snaplen (bytes)"
          hint="Per-packet capture limit. 262144 handles GRO super-frames; lower wastes nothing but truncates large LLM POST bodies."
        >
          <input
            type="number"
            min={64}
            max={1 << 20}
            value={source.snaplen}
            onChange={(e) =>
              onChange({ ...source, snaplen: Number(e.target.value) || DEFAULT_SNAPLEN })
            }
            className="w-32 rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
          />
        </Field>
      </Disclosure>
    </div>
  )
}

function StructuredFilter({
  ports,
  hosts,
  onChange,
}: {
  ports: number[]
  hosts: string[]
  onChange: (ports: number[], hosts: string[]) => void
}) {
  return (
    <div className="flex flex-col gap-2 text-xs">
      <div>
        <div className="mb-1 text-muted-foreground">Ports (TCP)</div>
        <ChipInput
          values={ports.map(String)}
          onChange={(next) =>
            onChange(
              next.map(Number).filter((n) => Number.isInteger(n)),
              hosts,
            )
          }
          placeholder="e.g. 4210, 4271 — press Enter or comma"
          validate={(t) => isValidPort(t)}
        />
      </div>
      <div>
        <div className="mb-1 text-muted-foreground">Hosts (IPv4 / hostname)</div>
        <ChipInput
          values={hosts}
          onChange={(next) => onChange(ports, next)}
          placeholder="e.g. 10.0.0.1 — leave empty to capture from any host"
          validate={(t) => isValidHost(t)}
        />
      </div>
      <div className="text-[11px] text-muted-foreground">
        Empty = capture all TCP. Multiple ports or hosts are ORed; ports AND hosts
        when both are set.
      </div>
    </div>
  )
}

function RawBpfInput({
  value,
  onChange,
  structuredAvailable,
  onBackToStructured,
}: {
  value: string
  onChange: (v: string) => void
  structuredAvailable: boolean
  onBackToStructured: () => void
}) {
  return (
    <div className="flex flex-col gap-2 text-xs">
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="Raw libpcap filter expression"
        className="w-full rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
      />
      <div className="text-[11px] text-muted-foreground">
        Free-form{" "}
        <a
          href="https://www.tcpdump.org/manpages/pcap-filter.7.html"
          target="_blank"
          rel="noreferrer"
          className="underline"
        >
          pcap-filter(7)
        </a>{" "}
        expression. Validated server-side before save.
      </div>
      {!structuredAvailable && value.trim() !== "" ? (
        <div className="flex items-start gap-1.5 rounded-md bg-muted/40 px-2 py-1.5 text-[11px] text-muted-foreground">
          <AlertCircle className="mt-0.5 size-3 shrink-0" />
          <span>
            This expression uses features beyond ports + hosts. The ports/hosts
            editor would silently lose them.
          </span>
        </div>
      ) : (
        structuredAvailable && (
          <button
            type="button"
            onClick={onBackToStructured}
            className="self-start text-[11px] text-primary underline"
          >
            ← back to ports/hosts editor
          </button>
        )
      )}
    </div>
  )
}

// ============================================================================
// CloudProbeForm
// ============================================================================

function CloudProbeForm({
  source,
  onChange,
}: {
  source: Extract<CaptureSource, { type: "cloud-probe" }>
  onChange: (next: CaptureSource) => void
}) {
  const [advancedOpen, setAdvancedOpen] = useState(false)
  // Endpoint is a ZMQ TCP URI like `tcp://0.0.0.0:5555`. Show a friendly
  // host:port input; we add/strip the `tcp://` prefix automatically.
  const friendly = source.endpoint.replace(/^tcp:\/\//, "")
  const updateEndpoint = (v: string) => {
    const trimmed = v.trim().replace(/^tcp:\/\//, "")
    onChange({ ...source, endpoint: trimmed === "" ? "" : `tcp://${trimmed}` })
  }
  return (
    <div className="flex flex-col gap-3">
      <Field
        label="Listen on"
        hint="ZMQ address heron will bind to receive packets from remote probes. Format host:port. 0.0.0.0 = all addresses on this host."
      >
        <div className="flex items-center">
          <span className="rounded-l-md border border-r-0 border-border bg-muted px-2 py-1 font-mono text-xs text-muted-foreground">
            tcp://
          </span>
          <input
            value={friendly}
            onChange={(e) => updateEndpoint(e.target.value)}
            placeholder="0.0.0.0:5555"
            className="flex-1 rounded-r-md border border-border bg-background px-2 py-1 font-mono text-xs"
          />
        </div>
      </Field>
      <Disclosure
        open={advancedOpen}
        onToggle={() => setAdvancedOpen((v) => !v)}
        label="Advanced"
      >
        <Field
          label="Receive queue depth"
          hint="ZMQ high-water mark. Caps how many in-flight messages the receiver buffers when downstream stages stall."
        >
          <input
            type="number"
            min={1}
            value={source.recv_hwm}
            onChange={(e) =>
              onChange({
                ...source,
                recv_hwm: Number(e.target.value) || DEFAULT_CLOUD_PROBE_HWM,
              })
            }
            className="w-32 rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
          />
        </Field>
      </Disclosure>
    </div>
  )
}

// ============================================================================
// PcapFileForm
// ============================================================================

function PcapFileForm({
  source,
  onChange,
}: {
  source: Extract<CaptureSource, { type: "pcap-file" }>
  onChange: (next: CaptureSource) => void
}) {
  return (
    <div className="flex flex-col gap-3">
      <Field
        label="File path"
        hint="Absolute path to a .pcap or .pcapng file readable by the heron process."
      >
        <input
          value={source.path}
          onChange={(e) => onChange({ ...source, path: e.target.value })}
          placeholder="/path/to/capture.pcap"
          className="w-full rounded-md border border-border bg-background px-2 py-1 font-mono text-xs"
        />
      </Field>
      <label className="flex items-center gap-2 text-xs">
        <input
          type="checkbox"
          checked={source.realtime}
          onChange={(e) => onChange({ ...source, realtime: e.target.checked })}
        />
        <span>Replay at original wall-clock speed (otherwise as fast as possible)</span>
      </label>
    </div>
  )
}

// ============================================================================
// Shared primitives
// ============================================================================

function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: string
  children: React.ReactNode
}) {
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-baseline justify-between gap-2">
        <label className="text-xs font-medium">{label}</label>
      </div>
      {children}
      {hint && <div className="text-[11px] text-muted-foreground">{hint}</div>}
    </div>
  )
}

function Disclosure({
  open,
  onToggle,
  label,
  children,
}: {
  open: boolean
  onToggle: () => void
  label: string
  children: React.ReactNode
}) {
  return (
    <div>
      <button
        type="button"
        onClick={onToggle}
        className="inline-flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        {label}
      </button>
      {open && <div className="mt-2">{children}</div>}
    </div>
  )
}

function formatInterfaceOption(i: CaptureInterface): string {
  const flags: string[] = []
  if (!i.is_up) flags.push("down")
  if (i.is_loopback) flags.push("loopback")
  if (i.is_wireless) flags.push("wireless")
  const flagStr = flags.length > 0 ? `  [${flags.join(", ")}]` : ""
  const addrStr = i.addresses.length > 0 ? `  ·  ${i.addresses[0]}` : ""
  return `${i.name}${addrStr}${flagStr}`
}
