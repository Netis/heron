export type ParsedRole = "system" | "user" | "assistant" | "tool"

export type ParsedContentBlock =
  | { type: "text"; text: string }
  | { type: "tool_use"; id: string; name: string; args_json: string }
  | { type: "tool_result"; tool_use_id: string; content: string; is_error: boolean }
  | { type: "image"; mime: string | null; size_bytes: number | null }
  | { type: "unknown"; raw: unknown }

export interface ParsedMessage {
  role: ParsedRole
  content: ParsedContentBlock[]
}

export interface ParsedToolDef {
  name: string
  description: string | null
  input_schema_json: string
}

export interface ParsedSampling {
  temperature: number | null
  max_tokens: number | null
  top_p: number | null
  top_k: number | null
  stream: boolean | null
  tool_choice: string | null
  stop: string[]
  response_format: string | null
}

export interface ParsedToolCall {
  id: string
  name: string
  args_json: string
}

export interface ParsedToolResult {
  tool_use_id: string
  content: string
  is_error: boolean
}

export interface ParsedInput {
  system: string | null
  messages: ParsedMessage[]
  tools: ParsedToolDef[]
  sampling: ParsedSampling
  user_message: string | null
  tool_results: ParsedToolResult[]
}

export interface ParsedOutput {
  reasoning: string | null
  message: string | null
  tool_calls: ParsedToolCall[]
}

export interface ParsedCall {
  input: ParsedInput
  output: ParsedOutput
  extensions?: Record<string, unknown>
}

export interface WireApiParser {
  parseInput(body: unknown): ParsedInput
  parseOutput(body: unknown): ParsedOutput
}

export function emptyInput(): ParsedInput {
  return {
    system: null,
    messages: [],
    tools: [],
    sampling: emptySampling(),
    user_message: null,
    tool_results: [],
  }
}

export function emptyOutput(): ParsedOutput {
  return { reasoning: null, message: null, tool_calls: [] }
}

export function emptySampling(): ParsedSampling {
  return {
    temperature: null,
    max_tokens: null,
    top_p: null,
    top_k: null,
    stream: null,
    tool_choice: null,
    stop: [],
    response_format: null,
  }
}
