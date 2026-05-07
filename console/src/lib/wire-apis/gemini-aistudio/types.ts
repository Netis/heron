/**
 * Gemini AI Studio (`generativelanguage.googleapis.com /v1beta/models/...:generateContent`)
 * native types. Field names mirror the Google GenAI SDK / public REST spec —
 * camelCase (`promptTokenCount`, `functionCall`) on purpose, no normalization
 * across providers. Future Code Assist OAuth and Vertex AI variants will land
 * as separate `gemini-codeassist` / `gemini-vertex` modules.
 */

// ── Parts (the Gemini equivalent of "content blocks") ────────────────────────

export type GeminiTextPart = {
  type: "text"
  text: string
}

/**
 * Gemini's "thinking" output. Wire format puts `thought: true` alongside `text`;
 * we keep them split for renderer dispatch.
 */
export type GeminiThoughtPart = {
  type: "thought"
  text: string
}

export type GeminiFunctionCallPart = {
  type: "function_call"
  name: string
  /** Args are a JSON-serializable object; we keep the raw value for display. */
  args: unknown
}

export type GeminiFunctionResponsePart = {
  type: "function_response"
  name: string
  response: unknown
}

export type GeminiInlineDataPart = {
  type: "inline_data"
  /** Wire field is `mimeType` (camelCase); we keep that. */
  mimeType: string
  data: string
}

export type GeminiUnknownPart = {
  type: "unknown"
  raw: unknown
}

export type GeminiPart =
  | GeminiTextPart
  | GeminiThoughtPart
  | GeminiFunctionCallPart
  | GeminiFunctionResponsePart
  | GeminiInlineDataPart
  | GeminiUnknownPart

// ── Roles & messages ─────────────────────────────────────────────────────────

export type GeminiRole = "user" | "model"

export interface GeminiContent {
  role: GeminiRole
  parts: GeminiPart[]
}

// ── Tools ────────────────────────────────────────────────────────────────────

export interface GeminiFunctionDeclaration {
  name: string
  description: string | null
  /** Wire ships `parametersJsonSchema` (preferred) or `parameters`. */
  parametersJsonSchema: unknown
}

// ── Generation config / sampling ─────────────────────────────────────────────

export interface GeminiThinkingConfig {
  thinkingLevel: string | null
  thinkingBudget: number | null
  includeThoughts: boolean | null
}

export interface GeminiGenerationConfig {
  temperature: number | null
  topP: number | null
  topK: number | null
  candidateCount: number | null
  maxOutputTokens: number | null
  thinkingConfig: GeminiThinkingConfig | null
}

// ── Usage ────────────────────────────────────────────────────────────────────

export interface GeminiUsageMetadata {
  promptTokenCount: number | null
  candidatesTokenCount: number | null
  totalTokenCount: number | null
  cachedContentTokenCount: number | null
  /**
   * Subset of candidatesTokenCount that was emitted as thinking output.
   * Don't add this to candidatesTokenCount or you'll double-count tokens.
   */
  thoughtsTokenCount: number | null
}

// ── Top-level parsed shape ───────────────────────────────────────────────────

export interface GeminiRequest {
  /** Gemini puts model in the URL path; we accept body fallback for completeness. */
  model: string | null
  systemInstruction: GeminiContent | null
  contents: GeminiContent[]
  tools: GeminiFunctionDeclaration[]
  generationConfig: GeminiGenerationConfig | null
}

/**
 * Gemini wire `finishReason` values, plus our synthetic `TOOL_USE` produced by
 * the backend extractor when a STOP response carries `functionCall` parts.
 */
export type GeminiFinishReason =
  | "STOP"
  | "MAX_TOKENS"
  | "SAFETY"
  | "RECITATION"
  | "LANGUAGE"
  | "BLOCKLIST"
  | "PROHIBITED_CONTENT"
  | "SPII"
  | "MALFORMED_FUNCTION_CALL"
  | "IMAGE_SAFETY"
  | "OTHER"
  | "TOOL_USE"
  | null

export interface GeminiCandidate {
  index: number | null
  finishReason: GeminiFinishReason | string | null
  content: GeminiContent
}

export interface GeminiResponse {
  responseId: string | null
  modelVersion: string | null
  candidates: GeminiCandidate[]
  usageMetadata: GeminiUsageMetadata
}

export interface GeminiAiStudioCall {
  request: GeminiRequest
  response: GeminiResponse
}
