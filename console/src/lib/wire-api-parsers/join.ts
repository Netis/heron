import type { ParsedToolCall, ParsedToolResult } from "./types"

export interface JoinedToolCall extends ParsedToolCall {
  result: ParsedToolResult | null
}

/**
 * Join tool_use blocks from the current call's output to tool_result blocks
 * from the next call's input (matched by tool_use_id / call_id). Missing
 * results stay as `null` — the UI can render "(no response, turn ended)".
 */
export function joinToolResults(
  toolCalls: ParsedToolCall[],
  nextCallToolResults: ParsedToolResult[],
): JoinedToolCall[] {
  return toolCalls.map((tc) => {
    const result = nextCallToolResults.find((tr) => tr.tool_use_id === tc.id) ?? null
    return { ...tc, result }
  })
}
