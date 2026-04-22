import { useMemo } from "react"
import { Wrench, MessageSquare, Target, Brain, FileSearch, Globe, Share2 } from "lucide-react"
import { cn } from "@/lib/utils"
import { parseOpenAiResponsesCall } from "@/lib/wire-apis/openai-responses"
import type { CallType } from "@/lib/wire-apis/call-type"

/**
 * List-view chip for an OpenAI Responses call. Shows:
 *   - base type with item-kind icons for file_search / web_search / mcp / function_call
 *   - reasoning badge if output contains reasoning items
 */
export interface OpenAiResponsesCallChipProps {
  responseBody: string | null | undefined
  callType: CallType
}

export function OpenAiResponsesCallChip({ responseBody, callType }: OpenAiResponsesCallChipProps) {
  const { toolNames, toolCount, hasReasoning, specialKinds } = useMemo(() => {
    const call = parseOpenAiResponsesCall(null, responseBody)
    const tools: string[] = []
    const kinds = new Set<string>()
    let reasoning = false
    for (const item of call.response.output) {
      if (item.kind === "function_call") tools.push(item.name)
      else if (item.kind === "reasoning") reasoning = true
      else if (item.kind === "file_search_call" || item.kind === "web_search_call" || item.kind === "mcp_call") {
        kinds.add(item.kind)
      }
    }
    return {
      toolNames: tools.slice(0, 2),
      toolCount: tools.length,
      hasReasoning: reasoning,
      specialKinds: kinds,
    }
  }, [responseBody])

  if (callType === "final") {
    return (
      <div className="flex items-center gap-1">
        <span className="flex items-center gap-1 rounded bg-emerald-100 px-1.5 py-0.5 text-[10px] font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300">
          <Target className="size-3" /> final
        </span>
        {hasReasoning && <ReasoningBadge />}
      </div>
    )
  }

  if (callType === "tool_call") {
    const more = toolCount - toolNames.length
    return (
      <div className="flex items-center gap-1">
        {toolNames.length > 0 && (
          <span className={cn(
            "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-medium",
            "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
          )}>
            <Wrench className="size-3" />
            {toolNames.join(", ")}
            {more > 0 && <span className="ml-1 opacity-70">+{more}</span>}
          </span>
        )}
        {specialKinds.has("file_search_call") && (
          <SpecialBadge icon={<FileSearch className="size-2.5" />} label="file_search" />
        )}
        {specialKinds.has("web_search_call") && (
          <SpecialBadge icon={<Globe className="size-2.5" />} label="web_search" />
        )}
        {specialKinds.has("mcp_call") && (
          <SpecialBadge icon={<Share2 className="size-2.5" />} label="mcp" />
        )}
        {hasReasoning && <ReasoningBadge />}
      </div>
    )
  }

  return (
    <div className="flex items-center gap-1">
      <span className="flex items-center gap-1 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
        <MessageSquare className="size-3" /> text
      </span>
      {hasReasoning && <ReasoningBadge />}
    </div>
  )
}

function ReasoningBadge() {
  return (
    <span
      title="response contains reasoning"
      className="flex items-center gap-0.5 rounded bg-purple-100 px-1 py-0.5 text-[9px] text-purple-700 dark:bg-purple-900/40 dark:text-purple-300"
    >
      <Brain className="size-2.5" />
    </span>
  )
}

function SpecialBadge({ icon, label }: { icon: React.ReactNode; label: string }) {
  return (
    <span
      title={label}
      className="flex items-center gap-0.5 rounded bg-sky-100 px-1 py-0.5 text-[9px] text-sky-700 dark:bg-sky-900/40 dark:text-sky-300"
    >
      {icon}
    </span>
  )
}
