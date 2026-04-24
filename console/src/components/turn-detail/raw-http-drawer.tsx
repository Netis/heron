import { X } from "lucide-react"
import { StatusBadge } from "@/components/ui/status-badge"
import { CollapsibleSection } from "@/components/ui/collapsible-section"
import { BodyViewer } from "@/components/raw-http/body-viewer"
import { parseHeaders } from "@/components/raw-http/helpers"
import { formatDateTimeMs, formatMs } from "@/lib/format"

export interface RawHttpData {
  request_path: string
  status_code: number | null
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  is_stream: boolean
  e2e_latency_ms: number | null
  request_time: number
  request_headers: string | null
  response_headers: string | null
  request_body: string | null
  response_body: string | null
}

interface Props {
  data: RawHttpData | null
  onClose: () => void
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
        <div className="min-h-0 flex-1 overflow-y-auto">
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
    <div className="flex flex-col">
      <RequestLine data={data} />
      <CollapsibleSection title="Request Headers" count={reqH.length} defaultOpen>
        <HeaderTable rows={reqH} />
      </CollapsibleSection>
      <CollapsibleSection title="Response Headers" count={respH.length} defaultOpen>
        <HeaderTable rows={respH} />
      </CollapsibleSection>
      <BodyViewer title="Request Body" raw={data.request_body} />
      <BodyViewer title="Response Body" raw={data.response_body} />
    </div>
  )
}

function RequestLine({ data }: { data: RawHttpData }) {
  return (
    <div className="flex flex-col gap-1 border-b border-border px-4 py-3 font-mono text-xs">
      <div className="flex items-center gap-2">
        <span className="font-semibold text-amber-300">POST</span>
        <span className="truncate" title={data.request_path}>{data.request_path}</span>
        <span className="text-muted-foreground">·</span>
        <StatusBadge status={data.status_code} />
      </div>
      <div className="text-muted-foreground">
        {data.client_ip}:{data.client_port} → {data.server_ip}:{data.server_port}
        {" · "}
        {data.is_stream ? "stream" : "non-stream"}
        {" · "}
        {formatMs(data.e2e_latency_ms)}
        {" · "}
        {formatDateTimeMs(data.request_time)}
      </div>
    </div>
  )
}

function HeaderTable({ rows }: { rows: [string, string][] }) {
  if (rows.length === 0) return <p className="text-sm text-muted-foreground">No headers</p>
  return (
    <table className="w-full text-sm">
      <tbody>
        {rows.map(([k, v], i) => (
          <tr key={i} className="border-b border-border/30">
            <td className="w-[200px] py-1 pr-3 font-mono text-xs text-muted-foreground">{k}</td>
            <td className="break-all py-1 font-mono text-xs">{v}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}
