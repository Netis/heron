import type { CallType } from "./call-type"
import { classifyAnthropicType } from "./anthropic/classify"
import { classifyGeminiAiStudioType } from "./gemini-aistudio/classify"
import { classifyOpenAiChatType } from "./openai-chat/classify"
import { classifyOpenAiResponsesType } from "./openai-responses/classify"

/**
 * Classify a single LLM call into one of {tool_call, text, final} for stats
 * aggregation. Dispatches by wire_api to the provider-specific classifier.
 *
 * Unknown wire_api falls back to "text" (conservative — it's not a tool_call
 * if we can't prove it, and not "final" unless the id matches).
 */
export function classifyType(
  wireApi: string,
  responseBody: string | null | undefined,
  callId: string,
  finalCallId: string | null | undefined,
): CallType {
  if (finalCallId != null && callId === finalCallId) return "final"
  switch (wireApi) {
    case "anthropic":
      return classifyAnthropicType(responseBody, callId, finalCallId)
    case "openai-chat":
      return classifyOpenAiChatType(responseBody, callId, finalCallId)
    case "openai-responses":
      return classifyOpenAiResponsesType(responseBody, callId, finalCallId)
    case "gemini-aistudio":
      return classifyGeminiAiStudioType(responseBody, callId, finalCallId)
    default:
      return "text"
  }
}
