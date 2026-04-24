import type { AgentTurnCallItem } from "@/types/api"
import { parseAnthropicCall } from "./wire-apis/anthropic"
import { parseOpenAiChatCall } from "./wire-apis/openai-chat"
import { parseOpenAiResponsesCall } from "./wire-apis/openai-responses"

export interface ToolOrigin {
  call_sequence: number
  call_id: string
  tool_name: string
  /** Canonical JSON string of the tool_use arguments. Echoed at the tool_result site. */
  args_json: string
}

export interface ToolResolution {
  call_sequence: number
  call_id: string
  is_error: boolean
  size_bytes: number
  content: string
}

export interface ToolIndexEntry {
  origin: ToolOrigin | null
  resolution: ToolResolution | null
}

export type ToolIndex = Map<string, ToolIndexEntry>

interface ToolUseBlock { id: string; name: string; args_json: string }
interface ToolResultBlock { tool_use_id: string; content: string; is_error: boolean }

function safeJsonStringify(v: unknown): string {
  try { return JSON.stringify(v, null, 2) } catch { return String(v) }
}

function* iterAnthropicToolUses(responseBody: string | null): Generator<ToolUseBlock> {
  if (!responseBody) return
  const call = parseAnthropicCall(null, responseBody)
  for (const block of call.response.content) {
    if (block.type === "tool_use") yield { id: block.id, name: block.name, args_json: safeJsonStringify(block.input) }
  }
}

function* iterAnthropicToolResults(requestBody: string | null): Generator<ToolResultBlock> {
  if (!requestBody) return
  const call = parseAnthropicCall(requestBody, null)
  for (const msg of call.request.messages) {
    for (const block of msg.content) {
      if (block.type === "tool_result") {
        const content = typeof block.content === "string"
          ? block.content
          : JSON.stringify(block.content)
        yield { tool_use_id: block.tool_use_id, content, is_error: block.is_error }
      }
    }
  }
}

function* iterOpenAiChatToolUses(responseBody: string | null): Generator<ToolUseBlock> {
  if (!responseBody) return
  const call = parseOpenAiChatCall(null, responseBody)
  for (const choice of call.response.choices) {
    for (const tc of choice.message.tool_calls ?? []) {
      // OpenAI-chat arguments is already a JSON string on the wire; keep it verbatim.
      yield { id: tc.id, name: tc.function.name, args_json: tc.function.arguments }
    }
  }
}

function* iterOpenAiChatToolResults(requestBody: string | null): Generator<ToolResultBlock> {
  if (!requestBody) return
  const call = parseOpenAiChatCall(requestBody, null)
  for (const msg of call.request.messages) {
    if (msg.role === "tool" && msg.tool_call_id) {
      const content = typeof msg.content === "string"
        ? msg.content
        : JSON.stringify(msg.content ?? "")
      // OpenAI-chat tool messages have no is_error flag on the wire — hardcode false.
      yield { tool_use_id: msg.tool_call_id, content, is_error: false }
    }
  }
}

function* iterOpenAiResponsesToolUses(responseBody: string | null): Generator<ToolUseBlock> {
  if (!responseBody) return
  const call = parseOpenAiResponsesCall(null, responseBody)
  for (const item of call.response.output) {
    if (item.kind === "function_call") {
      // Responses arguments is already a JSON string per spec; keep it verbatim.
      yield { id: item.call_id, name: item.name, args_json: item.arguments }
    }
  }
}

function* iterOpenAiResponsesToolResults(requestBody: string | null): Generator<ToolResultBlock> {
  if (!requestBody) return
  const call = parseOpenAiResponsesCall(requestBody, null)
  for (const item of call.request.input) {
    if (item.kind === "function_call_output") {
      const content = typeof item.output === "string" ? item.output : JSON.stringify(item.output)
      // OpenAI-responses function_call_output has no is_error flag on the wire — hardcode false.
      yield { tool_use_id: item.call_id, content, is_error: false }
    }
  }
}

function* iterToolUses(call: AgentTurnCallItem): Generator<ToolUseBlock> {
  switch (call.wire_api) {
    case "anthropic":        yield* iterAnthropicToolUses(call.response_body); break
    case "openai-chat":      yield* iterOpenAiChatToolUses(call.response_body); break
    case "openai-responses": yield* iterOpenAiResponsesToolUses(call.response_body); break
  }
}

function* iterToolResults(call: AgentTurnCallItem): Generator<ToolResultBlock> {
  switch (call.wire_api) {
    case "anthropic":        yield* iterAnthropicToolResults(call.request_body); break
    case "openai-chat":      yield* iterOpenAiChatToolResults(call.request_body); break
    case "openai-responses": yield* iterOpenAiResponsesToolResults(call.request_body); break
  }
}

function byteLength(s: string): number {
  return new Blob([s]).size
}

export function buildToolIndex(calls: AgentTurnCallItem[]): ToolIndex {
  const index: ToolIndex = new Map()

  // Pass 1: tool_use origins (response side). First-wins — turn history is
  // carried forward in subsequent request bodies, but tool_use only appears
  // in the assistant response where it was first emitted.
  for (const call of calls) {
    for (const tu of iterToolUses(call)) {
      if (index.has(tu.id)) continue
      index.set(tu.id, {
        origin: { call_sequence: call.sequence, call_id: call.id, tool_name: tu.name, args_json: tu.args_json },
        resolution: null,
      })
    }
  }

  // Pass 2: tool_result resolutions (request side). First-wins — call#N+1's
  // request carries tool_results, and so does every subsequent call's history.
  // Record the earliest call that carried each result.
  for (const call of calls) {
    for (const tr of iterToolResults(call)) {
      const existing = index.get(tr.tool_use_id)
      if (existing?.resolution) continue
      const entry = existing ?? { origin: null, resolution: null }
      entry.resolution = {
        call_sequence: call.sequence,
        call_id: call.id,
        is_error: tr.is_error,
        size_bytes: byteLength(tr.content),
        content: tr.content,
      }
      index.set(tr.tool_use_id, entry)
    }
  }

  return index
}

export type ToolUseState = "healthy" | "capture_gap"
export type ToolResultState = "healthy" | "orphan"

export function classifyToolUseState(entry: ToolIndexEntry): ToolUseState {
  return entry.resolution != null ? "healthy" : "capture_gap"
}

export function classifyToolResultState(entry: ToolIndexEntry): ToolResultState {
  return entry.origin == null ? "orphan" : "healthy"
}
