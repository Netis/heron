/**
 * Anthropic-native types. Field names and shapes mirror the public Messages API
 * spec (https://docs.anthropic.com/en/api/messages) — no normalization across
 * providers. Unknown block types / fields are preserved as `raw: unknown` so
 * future Anthropic additions render gracefully instead of being silently lost.
 */

// ── Content blocks ──────────────────────────────────────────────────────────

export type AnthropicCacheControl = { type: "ephemeral"; ttl?: string }

export type AnthropicTextBlock = {
  type: "text"
  text: string
  cache_control?: AnthropicCacheControl
  citations?: unknown[]
}

export type AnthropicToolUseBlock = {
  type: "tool_use"
  id: string
  name: string
  /** The tool input is a JSON-serializable object; we keep it as-is for renderer display. */
  input: unknown
  cache_control?: AnthropicCacheControl
}

export type AnthropicToolResultBlock = {
  type: "tool_result"
  tool_use_id: string
  /** Anthropic spec allows string OR array of sub-blocks; we keep raw. */
  content: string | Array<{ type: string; [k: string]: unknown }>
  is_error: boolean
  cache_control?: AnthropicCacheControl
}

export type AnthropicImageBlock = {
  type: "image"
  source:
    | { type: "base64"; media_type: string; data: string }
    | { type: "url"; url: string }
  cache_control?: AnthropicCacheControl
}

export type AnthropicDocumentBlock = {
  type: "document"
  source: unknown
  title?: string
  context?: string
  citations?: { enabled: boolean }
  cache_control?: AnthropicCacheControl
}

export type AnthropicThinkingBlock = {
  type: "thinking"
  thinking: string
  signature?: string
}

export type AnthropicRedactedThinkingBlock = {
  type: "redacted_thinking"
  data: string
}

export type AnthropicUnknownBlock = {
  type: "unknown"
  raw: unknown
}

export type AnthropicBlock =
  | AnthropicTextBlock
  | AnthropicToolUseBlock
  | AnthropicToolResultBlock
  | AnthropicImageBlock
  | AnthropicDocumentBlock
  | AnthropicThinkingBlock
  | AnthropicRedactedThinkingBlock
  | AnthropicUnknownBlock

// ── Messages ────────────────────────────────────────────────────────────────

export type AnthropicRole = "user" | "assistant"

export interface AnthropicMessage {
  role: AnthropicRole
  content: AnthropicBlock[]
}

// ── System ──────────────────────────────────────────────────────────────────

/**
 * `system` in Anthropic can be a plain string OR an array of text blocks
 * (to support per-segment cache_control). We preserve both forms so renderers
 * can visualize cache markers.
 */
export type AnthropicSystem =
  | { kind: "string"; text: string }
  | { kind: "blocks"; blocks: AnthropicTextBlock[] }

// ── Tools ───────────────────────────────────────────────────────────────────

export interface AnthropicToolDef {
  name: string
  description: string | null
  /** Raw JSON-serializable input_schema object (keep as value; renderer pretty-prints). */
  input_schema: unknown
  cache_control?: AnthropicCacheControl
}

// ── Sampling / control ──────────────────────────────────────────────────────

export interface AnthropicSampling {
  max_tokens: number | null
  temperature: number | null
  top_p: number | null
  top_k: number | null
  stream: boolean | null
  stop_sequences: string[]
  /**
   * tool_choice: string "auto" | "any" | "none" | object {type: "tool", name: X}.
   * We keep the raw value so the renderer can show it verbatim.
   */
  tool_choice: unknown
  /** metadata.user_id if set. */
  user_id: string | null
}

// ── Usage ───────────────────────────────────────────────────────────────────

export interface AnthropicUsage {
  input_tokens: number | null
  output_tokens: number | null
  cache_read_input_tokens: number | null
  cache_creation_input_tokens: number | null
}

// ── Top-level parsed shape ──────────────────────────────────────────────────

export interface AnthropicRequest {
  model: string | null
  system: AnthropicSystem | null
  messages: AnthropicMessage[]
  tools: AnthropicToolDef[]
  sampling: AnthropicSampling
  /** Count of blocks carrying cache_control markers across system/messages/tools. */
  cache_control_count: number
}

export type AnthropicStopReason =
  | "end_turn"
  | "tool_use"
  | "max_tokens"
  | "stop_sequence"
  | "pause_turn"
  | "refusal"
  | null

export interface AnthropicResponse {
  id: string | null
  model: string | null
  role: string | null
  content: AnthropicBlock[]
  stop_reason: AnthropicStopReason | string | null
  stop_sequence: string | null
  usage: AnthropicUsage
}

export interface AnthropicCall {
  request: AnthropicRequest
  response: AnthropicResponse
}
