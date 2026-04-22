import { useMemo } from "react"
import { Wrench, MessageSquare, Target, Brain, Activity } from "lucide-react"
import { cn } from "@/lib/utils"
import { parseOpenAiChatCall } from "@/lib/wire-apis/openai-chat"
import type { CallType } from "@/lib/wire-apis/call-type"

/**
 * List-view chip for an OpenAI Chat Completions call. Shows:
 *   - base type (tool_call / text / final) with tool names
 *   - reasoning indicator if reasoning_content is present (o1/o3 models)
 *   - logprobs indicator if logprobs were requested
 */
export interface OpenAiChatCallChipProps {
  responseBody: string | null | undefined
  callType: CallType
}

export function OpenAiChatCallChip({ responseBody, callType }: OpenAiChatCallChipProps) {
  const { toolNames, toolCount, hasReasoning, hasLogprobs } = useMemo(() => {
    const call = parseOpenAiChatCall(null, responseBody)
    const choice = call.response.choices[0]
    const tools = choice?.message.tool_calls?.map((t) => t.function.name) ?? []
    const reasoning = (choice?.message.reasoning_content?.length ?? 0) > 0
    const logprobs = (choice?.logprobs?.length ?? 0) > 0
    return {
      toolNames: tools.slice(0, 2),
      toolCount: tools.length,
      hasReasoning: reasoning,
      hasLogprobs: logprobs,
    }
  }, [responseBody])

  if (callType === "final") {
    return (
      <div className="flex items-center gap-1">
        <span className="flex items-center gap-1 rounded bg-emerald-100 px-1.5 py-0.5 text-[10px] font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300">
          <Target className="size-3" /> final
        </span>
        {hasReasoning && <ReasoningBadge />}
        {hasLogprobs && <LogprobsBadge />}
      </div>
    )
  }

  if (callType === "tool_call") {
    const more = toolCount - toolNames.length
    return (
      <div className="flex items-center gap-1">
        <span className={cn(
          "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-medium",
          "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
        )}>
          <Wrench className="size-3" />
          {toolNames.join(", ")}
          {more > 0 && <span className="ml-1 opacity-70">+{more}</span>}
        </span>
        {hasReasoning && <ReasoningBadge />}
        {hasLogprobs && <LogprobsBadge />}
      </div>
    )
  }

  return (
    <div className="flex items-center gap-1">
      <span className="flex items-center gap-1 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
        <MessageSquare className="size-3" /> text
      </span>
      {hasReasoning && <ReasoningBadge />}
      {hasLogprobs && <LogprobsBadge />}
    </div>
  )
}

function ReasoningBadge() {
  return (
    <span
      title="response has reasoning_content"
      className="flex items-center gap-0.5 rounded bg-purple-100 px-1 py-0.5 text-[9px] text-purple-700 dark:bg-purple-900/40 dark:text-purple-300"
    >
      <Brain className="size-2.5" />
    </span>
  )
}

function LogprobsBadge() {
  return (
    <span
      title="response includes logprobs"
      className="flex items-center gap-0.5 rounded bg-purple-100 px-1 py-0.5 text-[9px] text-purple-700 dark:bg-purple-900/40 dark:text-purple-300"
    >
      <Activity className="size-2.5" />
    </span>
  )
}
