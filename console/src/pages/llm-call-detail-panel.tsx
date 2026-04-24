import { useState } from "react"
import { X, ChevronUp, ChevronDown, Loader2, FileCode2 } from "lucide-react"
import { useLlmCallDetail } from "@/hooks/use-llm-call-detail"
import { RawHttpDrawer, type RawHttpData } from "@/components/turn-detail/raw-http-drawer"
import { SummaryCards } from "@/components/llm-call-detail/summary-cards"
import { TimelineBar } from "@/components/llm-call-detail/timeline-bar"
import { MetadataGrid } from "@/components/llm-call-detail/metadata-grid"
import { CallRendererDispatch } from "@/components/call-renderers/dispatch"
import type { LlmCallDetail } from "@/types/api"

function toRawHttpData(detail: LlmCallDetail): RawHttpData {
  return {
    request_path: detail.request_path,
    status_code: detail.status_code,
    client_ip: detail.client_ip,
    client_port: detail.client_port,
    server_ip: detail.server_ip,
    server_port: detail.server_port,
    is_stream: detail.is_stream,
    e2e_latency_ms: detail.e2e_latency_ms,
    request_time: detail.request_time,
    request_headers: detail.request_headers,
    response_headers: detail.response_headers,
    request_body: detail.request_body,
    response_body: detail.response_body,
  }
}

interface Props {
  id: string
  onClose: () => void
  onNavigate: (direction: "prev" | "next") => void
  hasPrev: boolean
  hasNext: boolean
}

export function LlmCallDetailPanel({ id, onClose, onNavigate, hasPrev, hasNext }: Props) {
  const { data: detail, isLoading, isError } = useLlmCallDetail(id)
  const [rawOpen, setRawOpen] = useState(false)

  return (
    <>
      <div className="fixed inset-0 z-40 bg-black/20" onClick={onClose} />

      <div className="fixed top-0 right-0 z-50 flex h-full w-[70%] min-w-[560px] flex-col border-l border-border bg-background shadow-xl animate-in slide-in-from-right duration-200">
        <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">
          <h2 className="text-sm font-semibold">LLM Call Detail</h2>
          <div className="flex items-center gap-1">
            <button
              onClick={() => setRawOpen(true)}
              disabled={!detail}
              className="mr-2 flex items-center gap-1.5 rounded-md border border-border px-2 py-1 text-xs text-foreground transition-colors hover:bg-muted disabled:opacity-30"
            >
              <FileCode2 className="size-3.5" />
              Raw HTTP
            </button>
            <button
              onClick={() => onNavigate("prev")}
              disabled={!hasPrev}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronUp className="size-4" />
            </button>
            <button
              onClick={() => onNavigate("next")}
              disabled={!hasNext}
              className="rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-30"
            >
              <ChevronDown className="size-4" />
            </button>
            <button
              onClick={onClose}
              className="ml-2 rounded p-1 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <X className="size-4" />
            </button>
          </div>
        </div>

        <div className="flex-1 overflow-y-auto">
          {isLoading ? (
            <div className="flex h-40 items-center justify-center">
              <Loader2 className="size-5 animate-spin text-muted-foreground" />
            </div>
          ) : isError || !detail ? (
            <div className="flex h-40 items-center justify-center text-destructive">
              Failed to load LLM call detail
            </div>
          ) : (
            <div className="flex flex-col gap-4 p-4">
              <SummaryCards detail={detail} />
              <TimelineBar detail={detail} />
              <MetadataGrid detail={detail} />
              <CallRendererDispatch
                wireApi={detail.wire_api}
                requestBody={detail.request_body}
                responseBody={detail.response_body}
                hasRequestBody={detail.request_body != null}
              />
            </div>
          )}
        </div>
      </div>

      <RawHttpDrawer data={rawOpen && detail ? toRawHttpData(detail) : null} onClose={() => setRawOpen(false)} />
    </>
  )
}
