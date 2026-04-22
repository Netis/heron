import { useMemo } from "react"
import { Wrench, MessageSquare, Target, Brain, Zap } from "lucide-react"
import { cn } from "@/lib/utils"
import { parseAnthropicCall } from "@/lib/wire-apis/anthropic"
import type { CallType } from "@/lib/wire-apis/call-type"

/**
 * List-view chip for an Anthropic call. Shows:
 *   - base type icon (tool_call / text / final) with tool names when applicable
 *   - a thinking indicator when the response contains any thinking block
 *   - a cache indicator when the response has cache_read_input_tokens > 0
 */
export interface AnthropicCallChipProps {
  responseBody: string | null | undefined
  callType: CallType
}

export function AnthropicCallChip({ responseBody, callType }: AnthropicCallChipProps) {
  const { toolNames, toolCount, hasThinking, cacheHit } = useMemo(() => {
    const call = parseAnthropicCall(null, responseBody)
    const tools: string[] = []
    let thinking = false
    for (const b of call.response.content) {
      if (b.type === "tool_use") tools.push(b.name)
      else if (b.type === "thinking") thinking = true
    }
    const cache = (call.response.usage.cache_read_input_tokens ?? 0) > 0
    return { toolNames: tools.slice(0, 2), toolCount: tools.length, hasThinking: thinking, cacheHit: cache }
  }, [responseBody])

  if (callType === "final") {
    return (
      <div className="flex items-center gap-1">
        <span className="flex items-center gap-1 rounded bg-emerald-100 px-1.5 py-0.5 text-[10px] font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300">
          <Target className="size-3" /> final
        </span>
        {hasThinking && <ThinkingBadge />}
        {cacheHit && <CacheBadge />}
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
        {hasThinking && <ThinkingBadge />}
        {cacheHit && <CacheBadge />}
      </div>
    )
  }

  return (
    <div className="flex items-center gap-1">
      <span className="flex items-center gap-1 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-800 dark:bg-blue-900/40 dark:text-blue-300">
        <MessageSquare className="size-3" /> text
      </span>
      {hasThinking && <ThinkingBadge />}
      {cacheHit && <CacheBadge />}
    </div>
  )
}

function ThinkingBadge() {
  return (
    <span
      title="response contains thinking"
      className="flex items-center gap-0.5 rounded bg-purple-100 px-1 py-0.5 text-[9px] text-purple-700 dark:bg-purple-900/40 dark:text-purple-300"
    >
      <Brain className="size-2.5" />
    </span>
  )
}

function CacheBadge() {
  return (
    <span
      title="prompt cache hit"
      className="flex items-center gap-0.5 rounded bg-purple-100 px-1 py-0.5 text-[9px] text-purple-700 dark:bg-purple-900/40 dark:text-purple-300"
    >
      <Zap className="size-2.5" />
    </span>
  )
}
