import type { CallType } from "../call-type"
import { asArray, asString, get, parseJsonOrNull } from "../shared"

/**
 * Classify an OpenAI Responses call into {tool_call, text, final}.
 *
 * - `final` when callId matches finalCallId
 * - `tool_call` when response.output[] contains any function_call /
 *   file_search_call / web_search_call / computer_call / mcp_call
 * - `text` otherwise
 */
const TOOL_ITEM_TYPES = new Set([
  "function_call",
  "file_search_call",
  "web_search_call",
  "computer_call",
  "mcp_call",
])

export function classifyOpenAiResponsesType(
  responseBody: string | null | undefined,
  callId: string,
  finalCallId: string | null | undefined,
): CallType {
  if (finalCallId != null && callId === finalCallId) return "final"
  const body = parseJsonOrNull(responseBody)
  const output = asArray(get(body, "output"))
  if (!output) return "text"
  for (const item of output) {
    const t = asString(get(item, "type"))
    if (t && TOOL_ITEM_TYPES.has(t)) return "tool_call"
  }
  return "text"
}
