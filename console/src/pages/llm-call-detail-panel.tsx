import { useState } from "react"
import { X, ChevronUp, ChevronDown, Loader2 } from "lucide-react"
import { useLlmCallDetail } from "@/hooks/use-llm-call-detail"
import { RawHttpDrawer } from "@/components/turn-detail/raw-http-drawer"
import { CallParsedOutput } from "@/components/call-parsed-output"
import { SummaryCards } from "@/components/llm-call-detail/summary-cards"
import { TimelineBar } from "@/components/llm-call-detail/timeline-bar"
import { MetadataGrid } from "@/components/llm-call-detail/metadata-grid"
import { InputSection } from "@/components/llm-call-detail/input-section"

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
      {/* Backdrop */}
      <div className="fixed inset-0 z-40 bg-black/20" onClick={onClose} />

      {/* Panel */}
      <div className="fixed top-0 right-0 z-50 flex h-full w-[70%] min-w-[560px] flex-col border-l border-border bg-background shadow-xl animate-in slide-in-from-right duration-200">
        {/* Header */}
        <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">
          <h2 className="text-sm font-semibold">LLM Call Detail</h2>
          <div className="flex items-center gap-1">
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

        {/* Content */}
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
              {/* ① Summary */}
              <SummaryCards detail={detail} />

              {/* ② Timeline */}
              <TimelineBar detail={detail} />

              {/* ③ Metadata */}
              <MetadataGrid detail={detail} />

              {/* ④ Input */}
              <InputSection
                parsedInput={detail.parsed_input}
                wireApi={detail.wire_api}
                hasRequestBody={detail.request_body != null}
                onOpenRawHttp={() => setRawOpen(true)}
              />

              {/* ⑤ Output */}
              <section className="border-l-2 border-emerald-500/40 pl-3">
                <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
                  Output
                </div>
                <CallParsedOutput parsed={detail.parsed} />
              </section>

              {/* ⑥ Raw HTTP link */}
              <div className="flex justify-end border-t border-border pt-3">
                <button
                  onClick={() => setRawOpen(true)}
                  className="text-xs text-muted-foreground hover:text-foreground hover:underline"
                >
                  View raw HTTP →
                </button>
              </div>
            </div>
          )}
        </div>
      </div>

      {/* Raw HTTP drawer */}
      <RawHttpDrawer callId={rawOpen ? id : null} onClose={() => setRawOpen(false)} />
    </>
  )
}
