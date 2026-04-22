import type { ParsedToolCall } from "./types"
import { emptyOutput } from "./types"
import { getParser } from "./index"
import { parseJsonOrNull } from "./shared"

export type CallType = "tool_call" | "text" | "final"

export interface CallPreview {
  type: CallType
  toolCalls: ParsedToolCall[]
  messagePreview: string | null
  hasReasoning: boolean
}

const MESSAGE_PREVIEW_LEN = 60

/**
 * Derive list-view preview fields for a single call from its raw response body.
 * Mirrors what the backend `enrich()` function used to return per call:
 *   - type: "final" if callId matches the turn's final_call_id; else "tool_call"
 *     when any tool_use block exists in the output; else "text"
 *   - toolCalls / messagePreview / hasReasoning: from parseOutput(responseBody)
 */
export function deriveCallPreview(
  wireApi: string,
  responseBody: string | null | undefined,
  callId: string,
  finalCallId: string | null | undefined,
): CallPreview {
  if (finalCallId != null && callId === finalCallId) {
    return { type: "final", toolCalls: [], messagePreview: null, hasReasoning: false }
  }
  const parser = getParser(wireApi)
  const val = parseJsonOrNull(responseBody)
  const out = parser ? parser.parseOutput(val) : emptyOutput()
  const type: CallType = out.tool_calls.length > 0 ? "tool_call" : "text"
  const messagePreview = out.message
    ? out.message.slice(0, MESSAGE_PREVIEW_LEN)
    : null
  return { type, toolCalls: out.tool_calls, messagePreview, hasReasoning: out.reasoning != null }
}
