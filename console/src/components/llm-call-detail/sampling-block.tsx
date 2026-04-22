import type { ParsedSampling } from "@/types/api"

interface Props {
  sampling: ParsedSampling
}

function pairs(s: ParsedSampling): string[] {
  const out: string[] = []
  if (s.temperature != null) out.push(`temp=${s.temperature}`)
  if (s.max_tokens != null) out.push(`max_tokens=${s.max_tokens}`)
  if (s.top_p != null) out.push(`top_p=${s.top_p}`)
  if (s.top_k != null) out.push(`top_k=${s.top_k}`)
  if (s.stream != null) out.push(`stream=${s.stream}`)
  if (s.tool_choice) out.push(`tool_choice=${s.tool_choice}`)
  if (s.stop.length > 0) out.push(`stop=${JSON.stringify(s.stop)}`)
  if (s.response_format) out.push(`response_format=${s.response_format}`)
  return out
}

export function SamplingBlock({ sampling }: Props) {
  const items = pairs(sampling)
  return (
    <div className="rounded border border-border/60 bg-background px-3 py-2 text-xs">
      <span className="font-medium">Sampling</span>
      <span className="ml-2 text-muted-foreground">
        {items.length > 0 ? items.join(" · ") : "(defaults)"}
      </span>
    </div>
  )
}
