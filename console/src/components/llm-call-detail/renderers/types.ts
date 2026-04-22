import type { JoinedToolCall, ParsedCall } from "@/lib/wire-api-parsers"

export interface CallRendererProps {
  parsed: ParsedCall
  /** Tool-use results joined from the next call in the same turn (if any). */
  joinedToolCalls: JoinedToolCall[]
  wireApi: string
  hasRequestBody: boolean
  onOpenRawHttp: () => void
}

export type CallRenderer = React.FC<CallRendererProps>
