import { anthropicParser } from "./anthropic"
import { openaiChatParser } from "./openai-chat"
import { openaiResponsesParser } from "./openai-responses"
import type { ParsedCall, ParsedInput, ParsedOutput, WireApiParser } from "./types"
import { emptyInput, emptyOutput } from "./types"
import { parseJsonOrNull } from "./shared"

export { parseJsonOrNull } from "./shared"

export const WIRE_API_ANTHROPIC = "anthropic"
export const WIRE_API_OPENAI_CHAT = "openai-chat"
export const WIRE_API_OPENAI_RESPONSES = "openai-responses"

const parsers: Record<string, WireApiParser> = {
  [WIRE_API_ANTHROPIC]: anthropicParser,
  [WIRE_API_OPENAI_CHAT]: openaiChatParser,
  [WIRE_API_OPENAI_RESPONSES]: openaiResponsesParser,
}

export function getParser(wireApi: string): WireApiParser | null {
  return parsers[wireApi] ?? null
}

export function parseCall(
  wireApi: string,
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
): ParsedCall {
  const parser = getParser(wireApi)
  const reqVal = parseJsonOrNull(requestBody)
  const respVal = parseJsonOrNull(responseBody)
  const input: ParsedInput = parser ? parser.parseInput(reqVal) : emptyInput()
  const output: ParsedOutput = parser ? parser.parseOutput(respVal) : emptyOutput()
  return { input, output }
}

export type { ParsedCall, ParsedInput, ParsedOutput, WireApiParser } from "./types"
export type {
  ParsedContentBlock,
  ParsedMessage,
  ParsedRole,
  ParsedSampling,
  ParsedToolCall,
  ParsedToolDef,
  ParsedToolResult,
} from "./types"
export { joinToolResults } from "./join"
export type { JoinedToolCall } from "./join"
export { deriveCallPreview } from "./preview"
export type { CallPreview, CallType } from "./preview"
