/**
 * OpenAI Responses API native types — mirror the public API spec
 * (https://platform.openai.com/docs/api-reference/responses).
 *
 * Responses differs substantially from Chat Completions: `input[]` and
 * `output[]` are typed arrays of items (messages, function_calls, reasoning,
 * file_search_call, web_search_call, computer_call, mcp_call). We preserve
 * all of them as discriminated unions so renderers can show each kind natively.
 */

// ── message content parts ──────────────────────────────────────────────────

export type ResponsesInputText = { type: "input_text"; text: string }
export type ResponsesOutputText = { type: "output_text"; text: string; annotations?: unknown[] }
export type ResponsesPlainText = { type: "text"; text: string }
export type ResponsesInputImage = {
  type: "input_image"
  image_url?: string
  file_id?: string
  detail?: "auto" | "low" | "high"
}
export type ResponsesInputFile = {
  type: "input_file"
  file_id?: string
  filename?: string
  file_data?: string
}
export type ResponsesRefusal = { type: "refusal"; refusal: string }
export type ResponsesUnknownPart = { type: "unknown"; raw: unknown }

export type ResponsesContentPart =
  | ResponsesInputText
  | ResponsesOutputText
  | ResponsesPlainText
  | ResponsesInputImage
  | ResponsesInputFile
  | ResponsesRefusal
  | ResponsesUnknownPart

// ── item kinds ─────────────────────────────────────────────────────────────

export type ResponsesRole = "system" | "developer" | "user" | "assistant"

export interface ResponsesMessageItem {
  kind: "message"
  role: ResponsesRole
  content: string | ResponsesContentPart[]
  /** Responses may include `id` on message items. */
  id?: string
  status?: string
}

export interface ResponsesFunctionCall {
  kind: "function_call"
  id?: string
  call_id: string
  name: string
  /** Arguments as a JSON string per spec. */
  arguments: string
  status?: string
}

export interface ResponsesFunctionCallOutput {
  kind: "function_call_output"
  id?: string
  call_id: string
  /** Output may be a string OR an object; we keep the original JSON-serializable value. */
  output: unknown
}

export interface ResponsesReasoningItem {
  kind: "reasoning"
  id?: string
  /** Each summary entry has `type: "summary_text"` + `text`. We flatten to strings. */
  summary: string[]
  /** When encrypted_content is present, we indicate it without attempting to decode. */
  encrypted_content?: string
  status?: string
}

export interface ResponsesFileSearchCall {
  kind: "file_search_call"
  id?: string
  queries?: string[]
  results?: unknown[]
  status?: string
}

export interface ResponsesWebSearchCall {
  kind: "web_search_call"
  id?: string
  status?: string
  action?: unknown
}

export interface ResponsesComputerCall {
  kind: "computer_call"
  id?: string
  action?: unknown
  status?: string
}

export interface ResponsesMcpCall {
  kind: "mcp_call"
  id?: string
  server_label?: string
  name?: string
  arguments?: string
  output?: unknown
  error?: string
  status?: string
}

export interface ResponsesUnknownItem {
  kind: "unknown"
  raw: unknown
}

export type ResponsesItem =
  | ResponsesMessageItem
  | ResponsesFunctionCall
  | ResponsesFunctionCallOutput
  | ResponsesReasoningItem
  | ResponsesFileSearchCall
  | ResponsesWebSearchCall
  | ResponsesComputerCall
  | ResponsesMcpCall
  | ResponsesUnknownItem

// ── tools ───────────────────────────────────────────────────────────────────

export interface ResponsesToolDef {
  /** `function`, `file_search`, `web_search_preview`, `computer_use_preview`, `mcp`, ... */
  type: string
  /** For function tools. */
  name?: string
  description?: string
  parameters?: unknown
  strict?: boolean
  /** For file_search. */
  vector_store_ids?: string[]
  /** For mcp. */
  server_label?: string
  server_url?: string
  /** Preserve raw tool definition for anything else. */
  raw?: unknown
}

// ── reasoning config (request side) ────────────────────────────────────────

export interface ResponsesReasoningConfig {
  effort: "minimal" | "low" | "medium" | "high" | string | null
  summary: "auto" | "concise" | "detailed" | string | null
}

// ── sampling / request control ─────────────────────────────────────────────

export interface ResponsesSampling {
  temperature: number | null
  max_output_tokens: number | null
  top_p: number | null
  stream: boolean | null
  tool_choice: unknown
  parallel_tool_calls: boolean | null
  previous_response_id: string | null
  store: boolean | null
  metadata: Record<string, string> | null
  truncation: string | null
  include: string[]
  user: string | null
  service_tier: string | null
}

// ── usage ───────────────────────────────────────────────────────────────────

export interface ResponsesUsage {
  input_tokens: number | null
  output_tokens: number | null
  total_tokens: number | null
  cached_input_tokens: number | null
  reasoning_tokens: number | null
}

// ── top-level ──────────────────────────────────────────────────────────────

export interface ResponsesRequest {
  model: string | null
  instructions: string | null
  input: ResponsesItem[]
  tools: ResponsesToolDef[]
  reasoning: ResponsesReasoningConfig | null
  sampling: ResponsesSampling
}

export type ResponsesStatus = "completed" | "incomplete" | "failed" | "cancelled" | "in_progress" | string | null

export interface ResponsesResponse {
  id: string | null
  model: string | null
  status: ResponsesStatus
  output: ResponsesItem[]
  usage: ResponsesUsage
  /** Aggregated output_text from message items (convenience). */
  output_text_aggregated: string
}

export interface OpenAiResponsesCall {
  request: ResponsesRequest
  response: ResponsesResponse
}
