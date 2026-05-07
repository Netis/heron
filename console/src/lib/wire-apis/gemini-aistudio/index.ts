import { asArray, asBoolean, asNumber, asString, asUint, get, parseJsonOrNull } from "../shared"
import type {
  GeminiAiStudioCall,
  GeminiCandidate,
  GeminiContent,
  GeminiFunctionDeclaration,
  GeminiGenerationConfig,
  GeminiPart,
  GeminiRequest,
  GeminiResponse,
  GeminiRole,
  GeminiThinkingConfig,
  GeminiUsageMetadata,
} from "./types"

export type { GeminiAiStudioCall } from "./types"

const EMPTY_USAGE: GeminiUsageMetadata = {
  promptTokenCount: null,
  candidatesTokenCount: null,
  totalTokenCount: null,
  cachedContentTokenCount: null,
  thoughtsTokenCount: null,
}

function parsePart(raw: unknown): GeminiPart {
  // functionCall part
  const fnCall = get(raw, "functionCall")
  if (fnCall != null && typeof fnCall === "object") {
    return {
      type: "function_call",
      name: asString(get(fnCall, "name")) ?? "",
      args: get(fnCall, "args"),
    }
  }
  // functionResponse part
  const fnResp = get(raw, "functionResponse")
  if (fnResp != null && typeof fnResp === "object") {
    return {
      type: "function_response",
      name: asString(get(fnResp, "name")) ?? "",
      response: get(fnResp, "response"),
    }
  }
  // inlineData part (image / file blob)
  const inline = get(raw, "inlineData")
  if (inline != null && typeof inline === "object") {
    return {
      type: "inline_data",
      mimeType: asString(get(inline, "mimeType")) ?? "",
      data: asString(get(inline, "data")) ?? "",
    }
  }
  // text or thought part
  const text = asString(get(raw, "text"))
  if (text != null) {
    if (asBoolean(get(raw, "thought")) === true) {
      return { type: "thought", text }
    }
    return { type: "text", text }
  }
  return { type: "unknown", raw }
}

function parseContent(raw: unknown): GeminiContent {
  const role = asString(get(raw, "role"))
  const parts = asArray(get(raw, "parts"))
  return {
    role: (role === "model" ? "model" : "user") as GeminiRole,
    parts: parts ? parts.map(parsePart) : [],
  }
}

function parseTools(v: unknown): GeminiFunctionDeclaration[] {
  // Wire shape: `tools: [{functionDeclarations: [...]}, {googleSearch: {}}, ...]`.
  // We only surface functionDeclarations (with names) for the renderer; built-in
  // tools (googleSearch, codeExecution) carry no metadata to display.
  const toolsArr = asArray(v)
  if (!toolsArr) return []
  const out: GeminiFunctionDeclaration[] = []
  for (const t of toolsArr) {
    const decls = asArray(get(t, "functionDeclarations"))
    if (!decls) continue
    for (const d of decls) {
      const name = asString(get(d, "name"))
      if (!name) continue
      out.push({
        name,
        description: asString(get(d, "description")),
        parametersJsonSchema: get(d, "parametersJsonSchema") ?? get(d, "parameters"),
      })
    }
  }
  return out
}

function parseThinkingConfig(v: unknown): GeminiThinkingConfig | null {
  if (v == null || typeof v !== "object") return null
  return {
    thinkingLevel: asString(get(v, "thinkingLevel")),
    thinkingBudget: asUint(get(v, "thinkingBudget")),
    includeThoughts: asBoolean(get(v, "includeThoughts")),
  }
}

function parseGenerationConfig(v: unknown): GeminiGenerationConfig | null {
  if (v == null || typeof v !== "object") return null
  return {
    temperature: asNumber(get(v, "temperature")),
    topP: asNumber(get(v, "topP")),
    topK: asUint(get(v, "topK")),
    candidateCount: asUint(get(v, "candidateCount")),
    maxOutputTokens: asUint(get(v, "maxOutputTokens")),
    thinkingConfig: parseThinkingConfig(get(v, "thinkingConfig")),
  }
}

function parseRequest(raw: unknown): GeminiRequest {
  const contentsArr = asArray(get(raw, "contents"))
  const contents: GeminiContent[] = contentsArr ? contentsArr.map(parseContent) : []

  const sysRaw = get(raw, "systemInstruction")
  const systemInstruction =
    sysRaw != null && typeof sysRaw === "object" ? parseContent(sysRaw) : null

  return {
    model: asString(get(raw, "model")),
    systemInstruction,
    contents,
    tools: parseTools(get(raw, "tools")),
    generationConfig: parseGenerationConfig(get(raw, "generationConfig")),
  }
}

function parseUsage(v: unknown): GeminiUsageMetadata {
  if (v == null || typeof v !== "object") return EMPTY_USAGE
  return {
    promptTokenCount: asUint(get(v, "promptTokenCount")),
    candidatesTokenCount: asUint(get(v, "candidatesTokenCount")),
    totalTokenCount: asUint(get(v, "totalTokenCount")),
    cachedContentTokenCount: asUint(get(v, "cachedContentTokenCount")),
    thoughtsTokenCount: asUint(get(v, "thoughtsTokenCount")),
  }
}

function parseResponse(raw: unknown): GeminiResponse {
  const candidatesArr = asArray(get(raw, "candidates"))
  const candidates: GeminiCandidate[] = candidatesArr
    ? candidatesArr.map((c): GeminiCandidate => ({
        index: asUint(get(c, "index")),
        finishReason: asString(get(c, "finishReason")),
        content: parseContent(get(c, "content")),
      }))
    : []
  return {
    responseId: asString(get(raw, "responseId")),
    modelVersion: asString(get(raw, "modelVersion")),
    candidates,
    usageMetadata: parseUsage(get(raw, "usageMetadata")),
  }
}

export function parseGeminiAiStudioCall(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
): GeminiAiStudioCall {
  const req = parseJsonOrNull(requestBody)
  const resp = parseJsonOrNull(responseBody)
  return {
    request: req
      ? parseRequest(req)
      : {
          model: null,
          systemInstruction: null,
          contents: [],
          tools: [],
          generationConfig: null,
        },
    response: resp
      ? parseResponse(resp)
      : {
          responseId: null,
          modelVersion: null,
          candidates: [],
          usageMetadata: EMPTY_USAGE,
        },
  }
}
