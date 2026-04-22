import type { CallType } from "../call-type"
import { asArray, get, parseJsonOrNull } from "../shared"

/**
 * Classify an OpenAI Chat Completions call into {tool_call, text, final}.
 *
 * - `final` when callId matches finalCallId
 * - `tool_call` when choices[0].message.tool_calls is non-empty
 * - `text` otherwise
 */
export function classifyOpenAiChatType(
  responseBody: string | null | undefined,
  callId: string,
  finalCallId: string | null | undefined,
): CallType {
  if (finalCallId != null && callId === finalCallId) return "final"
  const body = parseJsonOrNull(responseBody)
  const choices = asArray(get(body, "choices"))
  const msg = choices && choices.length > 0 ? get(choices[0], "message") : undefined
  const toolCalls = asArray(get(msg, "tool_calls"))
  if (toolCalls && toolCalls.length > 0) return "tool_call"
  return "text"
}
