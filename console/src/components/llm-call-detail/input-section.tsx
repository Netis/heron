import type { ParsedInput } from "@/types/api"
import { MessagesBlock } from "./messages-block"
import { SystemBlock } from "./system-block"
import { ToolsBlock } from "./tools-block"
import { SamplingBlock } from "./sampling-block"

interface Props {
  parsedInput: ParsedInput
  wireApi: string
  hasRequestBody: boolean
  onOpenRawHttp: () => void
}

function isEmpty(p: ParsedInput): boolean {
  return (
    p.messages.length === 0 &&
    !p.system &&
    p.tools.length === 0 &&
    p.sampling.temperature == null &&
    p.sampling.max_tokens == null &&
    p.sampling.top_p == null &&
    p.sampling.top_k == null &&
    p.sampling.stream == null &&
    !p.sampling.tool_choice &&
    p.sampling.stop.length === 0 &&
    !p.sampling.response_format
  )
}

export function InputSection({ parsedInput, wireApi, hasRequestBody, onOpenRawHttp }: Props) {
  const empty = isEmpty(parsedInput)
  return (
    <section className="border-l-2 border-muted-foreground/30 pl-3">
      <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
        Input
      </div>
      {!hasRequestBody ? (
        <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
          Request body not captured.
        </div>
      ) : empty ? (
        <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs">
          Could not parse request body as <span className="font-mono">{wireApi}</span>.
          <button onClick={onOpenRawHttp} className="ml-2 text-foreground hover:underline">View raw HTTP →</button>
        </div>
      ) : (
        <div className="space-y-2">
          <MessagesBlock messages={parsedInput.messages} />
          <SystemBlock system={parsedInput.system} />
          <ToolsBlock tools={parsedInput.tools} />
          <SamplingBlock sampling={parsedInput.sampling} />
        </div>
      )}
    </section>
  )
}
