import { X } from "lucide-react"
import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import { formatDateTimeMs, formatMs, formatNumber } from "@/lib/format"

export interface RawHttpData {
  id: string
  wire_api: string
  model: string
  status_code: number | null
  finish_reason: string | null
  ttft_ms: number | null
  e2e_latency_ms: number | null
  input_tokens: number | null
  output_tokens: number | null
  request_path: string
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  is_stream: boolean
  request_time: number
  request_body: string | null
  response_body: string | null
  request_headers: string | null
  response_headers: string | null
}

interface Props {
  data: RawHttpData | null
  onClose: () => void
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

export function RawHttpDrawer({ data, onClose }: Props) {
  if (!data) return null

  return (
    <>
      <div className="fixed inset-0 z-[55] bg-black/40" onClick={onClose} />
      <div className="fixed top-0 right-0 z-[60] flex h-full w-[min(720px,50vw)] flex-col border-l border-border bg-background shadow-2xl animate-in slide-in-from-right duration-200">
        <div className="flex h-10 shrink-0 items-center justify-between border-b border-border px-4">
          <h3 className="text-sm font-semibold">Raw HTTP</h3>
          <button onClick={onClose} className="rounded p-1 hover:bg-muted">
            <X className="size-4" />
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto p-4">
          <RawHttpBody data={data} />
        </div>
      </div>
    </>
  )
}

function RawHttpBody({ data }: { data: RawHttpData }) {
  const reqH = parseHeaders(data.request_headers)
  const respH = parseHeaders(data.response_headers)

  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 gap-3">
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">Wire API / Model</div>
          <div>{data.wire_api}</div>
          <div className="text-muted-foreground">{data.model}</div>
        </div>
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">Status / Finish</div>
          <div className="flex items-center gap-2">
            <StatusBadge status={data.status_code} />
            <FinishBadge reason={data.finish_reason} />
          </div>
        </div>
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">TTFT / E2E</div>
          <div className="tabular-nums">{formatMs(data.ttft_ms)} / {formatMs(data.e2e_latency_ms)}</div>
        </div>
        <div className="rounded-lg border border-border bg-muted/30 px-3 py-2 text-xs">
          <div className="text-muted-foreground">Tokens</div>
          <div className="tabular-nums">{formatNumber(data.input_tokens)}↑ / {formatNumber(data.output_tokens)}↓</div>
        </div>
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
        {[
          ["ID", data.id],
          ["Path", data.request_path],
          ["Client", `${data.client_ip}:${data.client_port}`],
          ["Server", `${data.server_ip}:${data.server_port}`],
          ["Stream", data.is_stream ? "Yes" : "No"],
          ["Req Time", formatDateTimeMs(data.request_time)],
        ].map(([k, v]) => (
          <div key={k} className="contents">
            <span className="text-muted-foreground">{k}</span>
            <span className="truncate font-mono text-xs" title={String(v)}>{v}</span>
          </div>
        ))}
      </div>
      <CollapsibleSection title="Request Headers" count={reqH.length}>
        {reqH.length ? (
          <table className="w-full text-sm">
            <tbody>
              {reqH.map(([k, v], i) => (
                <tr key={i} className="border-b border-border/30">
                  <td className="w-[200px] py-1 pr-3 font-mono text-xs text-muted-foreground">{k}</td>
                  <td className="break-all py-1 font-mono text-xs">{v}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : <p className="text-sm text-muted-foreground">No headers</p>}
      </CollapsibleSection>
      <CollapsibleSection title="Response Headers" count={respH.length}>
        {respH.length ? (
          <table className="w-full text-sm">
            <tbody>
              {respH.map(([k, v], i) => (
                <tr key={i} className="border-b border-border/30">
                  <td className="w-[200px] py-1 pr-3 font-mono text-xs text-muted-foreground">{k}</td>
                  <td className="break-all py-1 font-mono text-xs">{v}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : <p className="text-sm text-muted-foreground">No headers</p>}
      </CollapsibleSection>
      <CollapsibleSection title="Request Body">
        {data.request_body ? (
          <pre className="max-h-[400px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
            {formatJson(data.request_body)}
          </pre>
        ) : <p className="text-sm text-muted-foreground">No body</p>}
      </CollapsibleSection>
      <CollapsibleSection title="Response Body">
        {data.response_body ? (
          <pre className="max-h-[400px] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
            {formatJson(data.response_body)}
          </pre>
        ) : <p className="text-sm text-muted-foreground">No body</p>}
      </CollapsibleSection>
    </div>
  )
}
