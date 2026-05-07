import type { CallType } from "../call-type"
import { asArray, asString, get, parseJsonOrNull } from "../shared"

/**
 * Classify a Gemini AI Studio call into one of {tool_call, text, final}.
 *
 * - `final` when the call id matches the turn's final_call_id
 * - `tool_call` when any candidate's content has a `functionCall` part, OR
 *   when the backend has already synthesized `finishReason = "TOOL_USE"`
 *   (mirrors the rule in `wire_apis/gemini_aistudio.rs` so frontend stats
 *   stay consistent with backend ones)
 * - `text` otherwise
 */
export function classifyGeminiAiStudioType(
  responseBody: string | null | undefined,
  callId: string,
  finalCallId: string | null | undefined,
): CallType {
  if (finalCallId != null && callId === finalCallId) return "final"
  const body = parseJsonOrNull(responseBody)
  const candidates = asArray(get(body, "candidates"))
  if (!candidates) return "text"
  for (const c of candidates) {
    const finish = asString(get(c, "finishReason"))
    if (finish === "TOOL_USE") return "tool_call"
    const parts = asArray(get(get(c, "content"), "parts"))
    if (!parts) continue
    for (const p of parts) {
      if (get(p, "functionCall") != null) return "tool_call"
    }
  }
  return "text"
}
