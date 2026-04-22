import { Wrench, MessageSquare, Target } from "lucide-react"
import { cn } from "@/lib/utils"
import type { CallType } from "@/lib/wire-apis/call-type"
import { classifyType } from "@/lib/wire-apis/dispatch"
import { AnthropicCallChip } from "./anthropic-chip"
import { OpenAiChatCallChip } from "./openai-chat-chip"
import { OpenAiResponsesCallChip } from "./openai-responses-chip"

/**
 * List-view chip dispatcher. Routes by wire_api to the provider-specific
 * chip component. Gantt icon (tiny timeline glyph) uses the shared CallType
 * enum since 12px is too small for provider differentiation.
 */
export interface CallChipDispatchProps {
  wireApi: string
  callId: string
  responseBody: string | null | undefined
  finalCallId: string | null | undefined
}

export function CallChipDispatch({ wireApi, callId, responseBody, finalCallId }: CallChipDispatchProps) {
  const callType = classifyType(wireApi, responseBody, callId, finalCallId)
  switch (wireApi) {
    case "anthropic":
      return <AnthropicCallChip responseBody={responseBody} callType={callType} />
    case "openai-chat":
      return <OpenAiChatCallChip responseBody={responseBody} callType={callType} />
    case "openai-responses":
      return <OpenAiResponsesCallChip responseBody={responseBody} callType={callType} />
    default:
      return <GenericTypeChip callType={callType} />
  }
}

/** Used by gantt-nav for its 12px timeline glyph. */
export function GanttCallTypeIcon({ callType }: { callType: CallType }) {
  const cls = "size-3"
  if (callType === "tool_call") return <Wrench className={cn(cls, "text-amber-600")} />
  if (callType === "final") return <Target className={cn(cls, "text-emerald-600")} />
  return <MessageSquare className={cn(cls, "text-blue-600")} />
}

/** Simple chip used as fallback for wire_apis that don't yet have a dedicated chip. */
function GenericTypeChip({ callType }: { callType: CallType }) {
  if (callType === "final") {
    return (
      <span className="flex items-center gap-1 rounded bg-emerald-100 px-1.5 py-0.5 text-[10px] font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300">
        <Target className="size-3" /> final
      </span>
    )
  }
  if (callType === "tool_call") {
    return (
      <span className="flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-300">
        <Wrench className="size-3" /> tool
      </span>
    )
  }
  return (
    <span className="flex items-center gap-1 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
      <MessageSquare className="size-3" /> text
    </span>
  )
}
