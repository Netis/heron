import { X, ChevronUp, ChevronDown, Loader2, Microscope, Fingerprint, Database, Globe } from "lucide-react"
import { useRequestDetail } from "@/hooks/use-request-detail"
import { formatDateTime, formatMs, formatNumber } from "@/lib/format"
import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import { LabPanel } from "@/components/lab/LabPanel"
import { ImperialSeal } from "@/components/lab/ImperialSeal"
import type { CallDetail } from "@/types/api"
import { cn } from "@/lib/utils"

interface Props {
  id: string
  onClose: () => void
  onNavigate: (direction: "prev" | "next") => void
  hasPrev: boolean
  hasNext: boolean
}

function parseHeaders(raw: string | null): [string, string][] {
  if (!raw) return []
  try {
    return JSON.parse(raw)
  } catch {
    return []
  }
}

function formatJson(raw: string | null): string {
  if (!raw) return ""
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

function ForensicKpi({ label, children, icon: Icon }: { label: string; children: React.ReactNode; icon?: any }) {
  return (
    <div className="flex flex-col gap-1 p-3 rounded-lg border border-white/5 bg-white/2 hover:bg-white/5 transition-colors">
      <div className="flex items-center gap-2 mb-1">
        {Icon && <Icon className="size-3 text-muted-foreground/50" />}
        <span className="text-[9px] font-bold tracking-[0.2em] uppercase text-muted-foreground/60">{label}</span>
      </div>
      <div className="text-sm font-mono text-cyan-400 truncate">{children}</div>
    </div>
  )
}

function TimelineBar({ detail }: { detail: CallDetail }) {
  const { request_time, response_time, complete_time, ttfb_ms, e2e_latency_ms } = detail

  if (!response_time || !complete_time || !e2e_latency_ms) {
    return (
      <div className="rounded-lg border border-white/5 bg-white/2 px-4 py-3 text-xs text-muted-foreground font-mono">
        TIMELINE_DATA_INCOMPLETE
      </div>
    )
  }

  const ttfbRatio = (ttfb_ms ?? 0) / e2e_latency_ms
  const genRatio = 1 - ttfbRatio

  return (
    <div className="rounded-lg border border-white/10 bg-black/40 px-4 py-4 lab-scanline">
      <div className="mb-3 flex justify-between text-[10px] font-mono text-muted-foreground/60 uppercase tracking-tighter">
        <span>T_START: {formatDateTime(request_time)}</span>
        <span>T_END: {formatDateTime(complete_time)}</span>
      </div>
      <div className="flex h-4 overflow-hidden rounded-sm bg-white/5 p-[2px]">
        {ttfbRatio > 0 && (
          <div
            className="flex items-center justify-center bg-cyan-500/80 text-[8px] font-bold text-black uppercase"
            style={{ width: `${Math.max(ttfbRatio * 100, 8)}%` }}
          >
            TTFB
          </div>
        )}
        {genRatio > 0 && (
          <div
            className="flex items-center justify-center bg-emerald-500/80 text-[8px] font-bold text-black uppercase"
            style={{ width: `${Math.max(genRatio * 100, 8)}%` }}
          >
            GEN
          </div>
        )}
      </div>
      <div className="mt-3 flex gap-6 text-[10px] font-mono">
        <div className="flex items-center gap-2">
           <div className="w-2 h-2 bg-cyan-500" />
           <span className="text-muted-foreground">LATENCY_TTFB:</span>
           <span className="text-cyan-400">{formatMs(ttfb_ms)}</span>
        </div>
        <div className="flex items-center gap-2">
           <div className="w-2 h-2 bg-emerald-500" />
           <span className="text-muted-foreground">LATENCY_GEN:</span>
           <span className="text-emerald-400">{formatMs(e2e_latency_ms - (ttfb_ms ?? 0))}</span>
        </div>
        <div className="ml-auto">
           <span className="text-muted-foreground">TOTAL:</span>
           <span className="text-white ml-2">{formatMs(e2e_latency_ms)}</span>
        </div>
      </div>
    </div>
  )
}

function HeadersTable({ headers }: { headers: [string, string][] }) {
  return (
    <div className="rounded-md border border-white/5 bg-black/20 overflow-hidden">
      <table className="w-full text-[11px] font-mono">
        <thead className="bg-white/5 text-muted-foreground uppercase tracking-widest text-[9px]">
           <tr>
              <th className="py-2 px-3 text-left font-bold">Trace_Key</th>
              <th className="py-2 px-3 text-left font-bold">Value</th>
           </tr>
        </thead>
        <tbody>
          {headers.map(([key, value], i) => (
            <tr key={i} className="border-b border-white/5 hover:bg-white/5 transition-colors">
              <td className="w-[200px] py-1.5 px-3 text-muted-foreground/60">{key}</td>
              <td className="break-all py-1.5 px-3 text-cyan-400/80">{value}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export function RequestDetailPanel({ id, onClose, onNavigate, hasPrev, hasNext }: Props) {
  const { data: detail, isLoading, isError } = useRequestDetail(id)

  const requestHeaders = detail ? parseHeaders(detail.request_headers) : []
  const responseHeaders = detail ? parseHeaders(detail.response_headers) : []

  return (
    <>
      {/* Backdrop */}
      <div className="fixed inset-0 z-40 bg-black/60 backdrop-blur-sm transition-opacity" onClick={onClose} />

      {/* Panel */}
      <div className="fixed top-0 right-0 z-50 flex h-full w-[65%] min-w-[520px] flex-col border-l border-white/10 bg-[#080808]/95 backdrop-blur-2xl shadow-[0_0_50px_-12px_rgba(0,0,0,0.5)] animate-in slide-in-from-right duration-300">
        {/* Header */}
        <div className="flex shrink-0 items-center justify-between border-b border-white/5 px-6 py-4 bg-white/2">
          <div className="flex items-center gap-3">
             <Microscope className="size-4 text-cyan-400" />
             <h2 className="text-xs font-bold uppercase tracking-[0.3em] text-foreground">Forensic_Inspector_v1.0</h2>
             <div className="h-1 w-20 bg-cyan-500/10 rounded-full overflow-hidden">
                <div className="h-full bg-cyan-500 animate-[loading_2s_infinite]" />
             </div>
          </div>
          <div className="flex items-center gap-1">
            <button
              onClick={() => onNavigate("prev")}
              disabled={!hasPrev}
              className="rounded p-1.5 text-muted-foreground transition-colors hover:bg-white/5 hover:text-foreground disabled:opacity-20"
            >
              <ChevronUp className="size-4" />
            </button>
            <button
              onClick={() => onNavigate("next")}
              disabled={!hasNext}
              className="rounded p-1.5 text-muted-foreground transition-colors hover:bg-white/5 hover:text-foreground disabled:opacity-20"
            >
              <ChevronDown className="size-4" />
            </button>
            <div className="w-[1px] h-4 bg-white/10 mx-2" />
            <button
              onClick={onClose}
              className="rounded p-1.5 text-muted-foreground transition-colors hover:bg-rose-500/20 hover:text-rose-400"
            >
              <X className="size-4" />
            </button>
          </div>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto px-6 py-6 space-y-6">
          {isLoading ? (
            <div className="flex h-full items-center justify-center">
              <Loader2 className="size-6 animate-spin text-cyan-500" />
            </div>
          ) : isError || !detail ? (
            <div className="flex h-full items-center justify-center text-rose-400 font-mono text-sm">
              ERROR::FAILED_TO_INIT_TRACE_DATA
            </div>
          ) : (
            <>
              {/* Top Row: Meta Summary */}
              <div className="grid grid-cols-4 gap-4">
                <ForensicKpi label="Model_Signature" icon={Fingerprint}>
                  {detail.model || detail.wire_api}
                </ForensicKpi>
                <ForensicKpi label="Execution_State" icon={Activity}>
                   <div className="flex items-center gap-2 mt-1">
                    <StatusBadge status={detail.status_code} />
                    <FinishBadge reason={detail.finish_reason} />
                  </div>
                </ForensicKpi>
                <ForensicKpi label="Latency_Forensics" icon={Zap}>
                  <span className="text-foreground">{formatMs(detail.e2e_latency_ms)}</span>
                  <span className="text-[10px] text-muted-foreground ml-2">({formatMs(detail.ttfb_ms)} TTFB)</span>
                </ForensicKpi>
                <ForensicKpi label="Token_Density" icon={Database}>
                   <span className="text-emerald-400">{formatNumber(detail.output_tokens)}</span>
                   <span className="text-[10px] text-muted-foreground/50 mx-1">/</span>
                   <span className="text-muted-foreground">{formatNumber(detail.input_tokens)}</span>
                </ForensicKpi>
              </div>

              {/* Timeline Forensics */}
              <LabPanel title="Temporal Analysis Timeline">
                <TimelineBar detail={detail} />
              </LabPanel>

              {/* Integrity Verification Card */}
              <div className="grid grid-cols-12 gap-6 items-stretch">
                 <div className="col-span-8 space-y-4">
                    <LabPanel title="Trace_Metadata" className="h-full">
                       <div className="grid grid-cols-2 gap-x-8 gap-y-3 font-mono text-[11px]">
                          <div className="flex flex-col">
                             <span className="text-muted-foreground/40 text-[9px] uppercase tracking-tighter">SignalPath</span>
                             <span className="text-foreground truncate">{detail.request_path}</span>
                          </div>
                          <div className="flex flex-col">
                             <span className="text-muted-foreground/40 text-[9px] uppercase tracking-tighter">Client_Origin</span>
                             <span className="text-foreground">{detail.client_ip}</span>
                          </div>
                          <div className="flex flex-col">
                             <span className="text-muted-foreground/40 text-[9px] uppercase tracking-tighter">API_Protocol</span>
                             <span className="text-cyan-400">{detail.api_type.toUpperCase()}</span>
                          </div>
                          <div className="flex flex-col">
                             <span className="text-muted-foreground/40 text-[9px] uppercase tracking-tighter">Tenant_ID</span>
                             <span className="text-foreground">{detail.tenant_id || "NULL"}</span>
                          </div>
                       </div>
                    </LabPanel>
                 </div>
                 <div className="col-span-4">
                    <LabPanel title="Integrity" className="h-full flex items-center justify-center bg-emerald-500/[0.02]">
                       <div className="flex flex-col items-center text-center">
                          <ImperialSeal size={56} className="text-emerald-500/50 mb-2" />
                          <div className="text-[9px] font-bold text-emerald-500/60 tracking-widest uppercase">Validated</div>
                       </div>
                    </LabPanel>
                 </div>
              </div>

              {/* Data Payloads */}
              <div className="space-y-4">
                <CollapsibleSection title="Request_Headers" count={requestHeaders.length} className="bg-white/2 border border-white/5 rounded-lg overflow-hidden">
                  {requestHeaders.length > 0 ? (
                    <HeadersTable headers={requestHeaders} />
                  ) : (
                    <p className="p-4 text-xs font-mono text-muted-foreground/40 italic">NO_HEADER_DATA</p>
                  )}
                </CollapsibleSection>

                <CollapsibleSection title="Request_Payload" className="bg-white/2 border border-white/5 rounded-lg overflow-hidden">
                  {detail.request_body ? (
                    <pre className="max-h-[300px] overflow-auto rounded-md bg-black/60 p-4 font-mono text-[11px] text-cyan-400/70 border border-white/5">
                      {formatJson(detail.request_body)}
                    </pre>
                  ) : (
                    <p className="p-4 text-xs font-mono text-muted-foreground/40 italic">PAYLOAD_EMPTY</p>
                  )}
                </CollapsibleSection>

                <CollapsibleSection title="Response_Payload" className="bg-white/2 border border-white/5 rounded-lg overflow-hidden">
                  {detail.response_body ? (
                    <pre className="max-h-[500px] overflow-auto rounded-md bg-black/60 p-4 font-mono text-[11px] text-emerald-400/70 border border-white/5">
                      {formatJson(detail.response_body)}
                    </pre>
                  ) : (
                    <p className="p-4 text-xs font-mono text-muted-foreground/40 italic">PAYLOAD_EMPTY</p>
                  )}
                </CollapsibleSection>
              </div>
            </>
          )}
        </div>
      </div>
      
      <style>{`
        @keyframes loading {
          0% { transform: translateX(-100%); }
          100% { transform: translateX(200%); }
        }
      `}</style>
    </>
  )
}

