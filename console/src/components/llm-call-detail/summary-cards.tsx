import { StatusBadge } from "@/components/ui/status-badge"
import { FinishBadge } from "@/components/ui/finish-badge"
import { formatMs, formatNumber } from "@/lib/format"
import type { LlmCallDetail } from "@/types/api"

function SummaryCard({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1 rounded-lg border border-border bg-muted/30 px-3 py-2">
      <span className="text-xs text-muted-foreground">{label}</span>
      <div className="text-sm font-medium">{children}</div>
    </div>
  )
}

interface Props {
  detail: LlmCallDetail
}

export function SummaryCards({ detail }: Props) {
  return (
    <div className="grid grid-cols-4 gap-3">
      <SummaryCard label="Wire API / Model">
        <div>{detail.wire_api}</div>
        <div className="truncate text-xs text-muted-foreground" title={detail.model}>
          {detail.model}
        </div>
      </SummaryCard>
      <SummaryCard label="Status / Finish">
        <div className="flex items-center gap-2">
          <StatusBadge status={detail.status_code} />
          <FinishBadge reason={detail.finish_reason} />
        </div>
      </SummaryCard>
      <SummaryCard label="TTFT / E2E">
        <div className="tabular-nums">{formatMs(detail.ttft_ms)}</div>
        <div className="text-xs tabular-nums text-muted-foreground">
          {formatMs(detail.e2e_latency_ms)}
        </div>
      </SummaryCard>
      <SummaryCard label={detail.tokens_estimated ? "Tokens (estimated)" : "Tokens"}>
        <div
          className="flex items-center gap-3 tabular-nums"
          title={
            detail.tokens_estimated
              ? "Estimated by tokenizer (cl100k) — server returned no usage block"
              : undefined
          }
        >
          <span className="flex flex-col">
            <span className="text-[10px] text-muted-foreground">in</span>
            <span className={detail.tokens_estimated ? "text-amber-700 dark:text-amber-400" : ""}>
              {detail.tokens_estimated ? "~" : ""}
              {formatNumber(detail.input_tokens)}
            </span>
          </span>
          <span className="flex flex-col">
            <span className="text-[10px] text-muted-foreground">out</span>
            <span className={detail.tokens_estimated ? "text-amber-700 dark:text-amber-400" : ""}>
              {detail.tokens_estimated ? "~" : ""}
              {formatNumber(detail.output_tokens)}
            </span>
          </span>
        </div>
        <div className="text-xs tabular-nums text-muted-foreground">
          total: {detail.tokens_estimated ? "~" : ""}
          {formatNumber(detail.total_tokens)}
        </div>
      </SummaryCard>
    </div>
  )
}
