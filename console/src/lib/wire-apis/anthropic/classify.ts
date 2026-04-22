import type { CallType } from "../call-type"
import { asArray, asString, get, parseJsonOrNull } from "../shared"

/**
 * Classify an Anthropic call into one of {tool_call, text, final}.
 *
 * - `final` when the call id matches the turn's final_call_id
 * - `tool_call` when the response content has at least one tool_use block
 * - `text` otherwise
 */
export function classifyAnthropicType(
  responseBody: string | null | undefined,
  callId: string,
  finalCallId: string | null | undefined,
): CallType {
  if (finalCallId != null && callId === finalCallId) return "final"
  const body = parseJsonOrNull(responseBody)
  const content = asArray(get(body, "content"))
  if (!content) return "text"
  for (const b of content) {
    if (asString(get(b, "type")) === "tool_use") return "tool_call"
  }
  return "text"
}
