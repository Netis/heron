/**
 * OpenAI Chat Completions native types — mirror the public API spec
 * (https://platform.openai.com/docs/api-reference/chat/create).
 *
 * No normalization: `role: "system" | "developer" | "user" | "assistant" | "tool"`
 * is kept distinct from Anthropic's roles. Unknown content part types are
 * preserved as `{type: "unknown", raw}` for forward-compat.
 */

export type OpenAiChatRole = "system" | "developer" | "user" | "assistant" | "tool"

// ── Content parts (for multimodal user messages) ───────────────────────────

export type OpenAiChatTextPart = { type: "text"; text: string }
export type OpenAiChatImagePart = {
  type: "image_url"
  image_url: { url: string; detail?: "auto" | "low" | "high" }
}
export type OpenAiChatAudioPart = {
  type: "input_audio"
  input_audio: { data: string; format: string }
}
export type OpenAiChatUnknownPart = { type: "unknown"; raw: unknown }

export type OpenAiChatPart =
  | OpenAiChatTextPart
  | OpenAiChatImagePart
  | OpenAiChatAudioPart
  | OpenAiChatUnknownPart

// ── Tool call (assistant-side) ─────────────────────────────────────────────

export interface OpenAiChatToolCall {
  id: string
  type: "function"
  function: {
    name: string
    /** Arguments are a JSON string (per OpenAI spec — kept verbatim). */
    arguments: string
  }
}

// ── Messages ────────────────────────────────────────────────────────────────

export type OpenAiChatMessageContent = string | OpenAiChatPart[] | null

export interface OpenAiChatMessage {
  role: OpenAiChatRole
  content: OpenAiChatMessageContent
  /** Assistant-only. */
  tool_calls?: OpenAiChatToolCall[]
  /** Assistant-only (reasoning models). */
  reasoning_content?: string
  /** Tool-role only. */
  tool_call_id?: string
  /** Name of the tool (function-role legacy / tool role supplement). */
  name?: string
  /** Refusal string (safety). */
  refusal?: string
}

// ── Tools ───────────────────────────────────────────────────────────────────

export interface OpenAiChatToolDef {
  type: "function"
  function: {
    name: string
    description: string | null
    /** Raw JSON schema object. */
    parameters: unknown
    /** Newer OpenAI structured-outputs flag. */
    strict?: boolean
  }
}

// ── Response format ─────────────────────────────────────────────────────────

export type OpenAiChatResponseFormat =
  | { kind: "text" }
  | { kind: "json_object" }
  | { kind: "json_schema"; name: string; schema: unknown; strict?: boolean; description?: string }
  | { kind: "unknown"; raw: unknown }
  | null

// ── Sampling ────────────────────────────────────────────────────────────────

export interface OpenAiChatSampling {
  temperature: number | null
  /** Modern name. */
  max_completion_tokens: number | null
  /** Legacy name (accepted by some deployments). */
  max_tokens: number | null
  top_p: number | null
  n: number | null
  seed: number | null
  stream: boolean | null
  stream_include_usage: boolean | null
  stop: string[]
  tool_choice: unknown
  parallel_tool_calls: boolean | null
  frequency_penalty: number | null
  presence_penalty: number | null
  logit_bias: Record<string, number> | null
  logprobs: boolean | null
  top_logprobs: number | null
  service_tier: string | null
  user: string | null
  /** Optional store + metadata (store=true persists on OpenAI side). */
  store: boolean | null
  metadata: Record<string, string> | null
}

// ── Choice / response ──────────────────────────────────────────────────────

export type OpenAiChatFinishReason =
  | "stop"
  | "length"
  | "tool_calls"
  | "function_call"
  | "content_filter"
  | null

export interface OpenAiChatLogprobEntry {
  token: string
  logprob: number
  bytes: number[] | null
  top_logprobs: Array<{ token: string; logprob: number; bytes: number[] | null }>
}

export interface OpenAiChatChoice {
  index: number
  message: OpenAiChatMessage
  finish_reason: OpenAiChatFinishReason | string | null
  /** Per-token logprobs for the content tokens, if requested. */
  logprobs: OpenAiChatLogprobEntry[] | null
}

export interface OpenAiChatUsage {
  prompt_tokens: number | null
  completion_tokens: number | null
  total_tokens: number | null
  /** prompt_tokens_details.cached_tokens, if present. */
  cached_prompt_tokens: number | null
  /** completion_tokens_details.reasoning_tokens, if present. */
  reasoning_tokens: number | null
}

// ── Top-level parsed shape ─────────────────────────────────────────────────

export interface OpenAiChatRequest {
  model: string | null
  messages: OpenAiChatMessage[]
  tools: OpenAiChatToolDef[]
  response_format: OpenAiChatResponseFormat
  sampling: OpenAiChatSampling
}

export interface OpenAiChatResponse {
  id: string | null
  model: string | null
  system_fingerprint: string | null
  service_tier: string | null
  choices: OpenAiChatChoice[]
  usage: OpenAiChatUsage
}

export interface OpenAiChatCall {
  request: OpenAiChatRequest
  response: OpenAiChatResponse
}
