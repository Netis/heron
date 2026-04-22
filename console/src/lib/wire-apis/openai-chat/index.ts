import { asArray, asBoolean, asNumber, asString, asUint, get, parseJsonOrNull } from "../shared"
import type {
  OpenAiChatCall,
  OpenAiChatChoice,
  OpenAiChatLogprobEntry,
  OpenAiChatMessage,
  OpenAiChatMessageContent,
  OpenAiChatPart,
  OpenAiChatRequest,
  OpenAiChatResponse,
  OpenAiChatResponseFormat,
  OpenAiChatRole,
  OpenAiChatSampling,
  OpenAiChatToolCall,
  OpenAiChatToolDef,
  OpenAiChatUsage,
} from "./types"

export type { OpenAiChatCall } from "./types"

const EMPTY_SAMPLING: OpenAiChatSampling = {
  temperature: null,
  max_completion_tokens: null,
  max_tokens: null,
  top_p: null,
  n: null,
  seed: null,
  stream: null,
  stream_include_usage: null,
  stop: [],
  tool_choice: undefined,
  parallel_tool_calls: null,
  frequency_penalty: null,
  presence_penalty: null,
  logit_bias: null,
  logprobs: null,
  top_logprobs: null,
  service_tier: null,
  user: null,
  store: null,
  metadata: null,
}

const EMPTY_USAGE: OpenAiChatUsage = {
  prompt_tokens: null,
  completion_tokens: null,
  total_tokens: null,
  cached_prompt_tokens: null,
  reasoning_tokens: null,
}

// ── content parts ──────────────────────────────────────────────────────────

function parseContent(raw: unknown): OpenAiChatMessageContent {
  const s = asString(raw)
  if (s != null) return s
  const arr = asArray(raw)
  if (!arr) return null
  const parts: OpenAiChatPart[] = []
  for (const item of arr) {
    const t = asString(get(item, "type"))
    if (t === "text") {
      parts.push({ type: "text", text: asString(get(item, "text")) ?? "" })
    } else if (t === "image_url") {
      const url = asString(get(get(item, "image_url"), "url")) ?? ""
      const detail = asString(get(get(item, "image_url"), "detail"))
      parts.push({
        type: "image_url",
        image_url: detail != null && (detail === "auto" || detail === "low" || detail === "high")
          ? { url, detail }
          : { url },
      })
    } else if (t === "input_audio") {
      const a = get(item, "input_audio")
      parts.push({
        type: "input_audio",
        input_audio: {
          data: asString(get(a, "data")) ?? "",
          format: asString(get(a, "format")) ?? "",
        },
      })
    } else {
      parts.push({ type: "unknown", raw: item })
    }
  }
  return parts
}

// ── tool calls ─────────────────────────────────────────────────────────────

function parseToolCalls(raw: unknown): OpenAiChatToolCall[] {
  const arr = asArray(raw)
  if (!arr) return []
  const out: OpenAiChatToolCall[] = []
  for (const tc of arr) {
    const id = asString(get(tc, "id")) ?? ""
    const f = get(tc, "function")
    out.push({
      id,
      type: "function",
      function: {
        name: asString(get(f, "name")) ?? "",
        arguments: asString(get(f, "arguments")) ?? "",
      },
    })
  }
  return out
}

// ── messages ────────────────────────────────────────────────────────────────

function parseMessages(raw: unknown): OpenAiChatMessage[] {
  const arr = asArray(raw)
  if (!arr) return []
  const out: OpenAiChatMessage[] = []
  for (const m of arr) {
    const role = asString(get(m, "role"))
    if (role == null) continue
    const normalizedRole = (["system", "developer", "user", "assistant", "tool"].includes(role)
      ? (role as OpenAiChatRole)
      : null) ?? null
    if (normalizedRole == null) continue
    const msg: OpenAiChatMessage = {
      role: normalizedRole,
      content: parseContent(get(m, "content")),
    }
    const toolCalls = parseToolCalls(get(m, "tool_calls"))
    if (toolCalls.length > 0) msg.tool_calls = toolCalls
    const reasoning = asString(get(m, "reasoning_content"))
    if (reasoning != null && reasoning.length > 0) msg.reasoning_content = reasoning
    const toolCallId = asString(get(m, "tool_call_id"))
    if (toolCallId != null) msg.tool_call_id = toolCallId
    const name = asString(get(m, "name"))
    if (name != null) msg.name = name
    const refusal = asString(get(m, "refusal"))
    if (refusal != null) msg.refusal = refusal
    out.push(msg)
  }
  return out
}

// ── tools ───────────────────────────────────────────────────────────────────

function parseTools(raw: unknown): OpenAiChatToolDef[] {
  const arr = asArray(raw)
  if (!arr) return []
  const out: OpenAiChatToolDef[] = []
  for (const t of arr) {
    if (asString(get(t, "type")) !== "function") continue
    const f = get(t, "function")
    const name = asString(get(f, "name"))
    if (!name) continue
    const strict = asBoolean(get(f, "strict"))
    out.push({
      type: "function",
      function: {
        name,
        description: asString(get(f, "description")),
        parameters: get(f, "parameters"),
        ...(strict != null ? { strict } : {}),
      },
    })
  }
  return out
}

// ── response_format ────────────────────────────────────────────────────────

function parseResponseFormat(raw: unknown): OpenAiChatResponseFormat {
  if (raw == null) return null
  const t = asString(get(raw, "type"))
  if (t === "text") return { kind: "text" }
  if (t === "json_object") return { kind: "json_object" }
  if (t === "json_schema") {
    const schema = get(raw, "json_schema")
    return {
      kind: "json_schema",
      name: asString(get(schema, "name")) ?? "",
      schema: get(schema, "schema"),
      strict: asBoolean(get(schema, "strict")) ?? undefined,
      description: asString(get(schema, "description")) ?? undefined,
    }
  }
  return { kind: "unknown", raw }
}

// ── sampling ────────────────────────────────────────────────────────────────

function parseStop(v: unknown): string[] {
  const s = asString(v)
  if (s != null) return [s]
  const arr = asArray(v)
  if (!arr) return []
  const out: string[] = []
  for (const x of arr) {
    const s2 = asString(x)
    if (s2 != null) out.push(s2)
  }
  return out
}

function parseLogitBias(v: unknown): Record<string, number> | null {
  if (v == null || typeof v !== "object" || Array.isArray(v)) return null
  const out: Record<string, number> = {}
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    const n = asNumber(val)
    if (n != null) out[k] = n
  }
  return Object.keys(out).length > 0 ? out : null
}

function parseMetadata(v: unknown): Record<string, string> | null {
  if (v == null || typeof v !== "object" || Array.isArray(v)) return null
  const out: Record<string, string> = {}
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    const s = asString(val)
    if (s != null) out[k] = s
  }
  return Object.keys(out).length > 0 ? out : null
}

function parseSampling(body: unknown): OpenAiChatSampling {
  return {
    temperature: asNumber(get(body, "temperature")),
    max_completion_tokens: asUint(get(body, "max_completion_tokens")),
    max_tokens: asUint(get(body, "max_tokens")),
    top_p: asNumber(get(body, "top_p")),
    n: asUint(get(body, "n")),
    seed: asUint(get(body, "seed")),
    stream: asBoolean(get(body, "stream")),
    stream_include_usage: asBoolean(get(get(body, "stream_options"), "include_usage")),
    stop: parseStop(get(body, "stop")),
    tool_choice: get(body, "tool_choice"),
    parallel_tool_calls: asBoolean(get(body, "parallel_tool_calls")),
    frequency_penalty: asNumber(get(body, "frequency_penalty")),
    presence_penalty: asNumber(get(body, "presence_penalty")),
    logit_bias: parseLogitBias(get(body, "logit_bias")),
    logprobs: asBoolean(get(body, "logprobs")),
    top_logprobs: asUint(get(body, "top_logprobs")),
    service_tier: asString(get(body, "service_tier")),
    user: asString(get(body, "user")),
    store: asBoolean(get(body, "store")),
    metadata: parseMetadata(get(body, "metadata")),
  }
}

// ── response ────────────────────────────────────────────────────────────────

function parseLogprobs(raw: unknown): OpenAiChatLogprobEntry[] | null {
  const arr = asArray(get(raw, "content"))
  if (!arr) return null
  const out: OpenAiChatLogprobEntry[] = []
  for (const item of arr) {
    const token = asString(get(item, "token"))
    const logprob = asNumber(get(item, "logprob"))
    if (token == null || logprob == null) continue
    const bytes = asArray(get(item, "bytes"))
      ?.map((b) => asUint(b))
      .filter((n): n is number => n != null) ?? null
    const topArr = asArray(get(item, "top_logprobs"))
    const top: OpenAiChatLogprobEntry["top_logprobs"] = []
    if (topArr) {
      for (const t of topArr) {
        const tToken = asString(get(t, "token"))
        const tLog = asNumber(get(t, "logprob"))
        if (tToken == null || tLog == null) continue
        const tBytes = asArray(get(t, "bytes"))
          ?.map((b) => asUint(b))
          .filter((n): n is number => n != null) ?? null
        top.push({ token: tToken, logprob: tLog, bytes: tBytes })
      }
    }
    out.push({ token, logprob, bytes, top_logprobs: top })
  }
  return out.length > 0 ? out : null
}

function parseChoices(raw: unknown): OpenAiChatChoice[] {
  const arr = asArray(raw)
  if (!arr) return []
  const out: OpenAiChatChoice[] = []
  for (const c of arr) {
    const msgRaw = get(c, "message")
    const role = asString(get(msgRaw, "role"))
    const normalizedRole = role === "assistant" || role === "tool" ? (role as OpenAiChatRole) : "assistant"
    const message: OpenAiChatMessage = {
      role: normalizedRole,
      content: parseContent(get(msgRaw, "content")),
    }
    const tc = parseToolCalls(get(msgRaw, "tool_calls"))
    if (tc.length > 0) message.tool_calls = tc
    const reasoning = asString(get(msgRaw, "reasoning_content"))
    if (reasoning != null && reasoning.length > 0) message.reasoning_content = reasoning
    const refusal = asString(get(msgRaw, "refusal"))
    if (refusal != null) message.refusal = refusal
    out.push({
      index: asUint(get(c, "index")) ?? 0,
      message,
      finish_reason: asString(get(c, "finish_reason")),
      logprobs: parseLogprobs(get(c, "logprobs")),
    })
  }
  return out
}

function parseUsage(raw: unknown): OpenAiChatUsage {
  if (!raw) return EMPTY_USAGE
  return {
    prompt_tokens: asUint(get(raw, "prompt_tokens")),
    completion_tokens: asUint(get(raw, "completion_tokens")),
    total_tokens: asUint(get(raw, "total_tokens")),
    cached_prompt_tokens: asUint(get(get(raw, "prompt_tokens_details"), "cached_tokens")),
    reasoning_tokens: asUint(get(get(raw, "completion_tokens_details"), "reasoning_tokens")),
  }
}

function parseRequest(raw: unknown): OpenAiChatRequest {
  return {
    model: asString(get(raw, "model")),
    messages: parseMessages(get(raw, "messages")),
    tools: parseTools(get(raw, "tools")),
    response_format: parseResponseFormat(get(raw, "response_format")),
    sampling: parseSampling(raw),
  }
}

function parseResponse(raw: unknown): OpenAiChatResponse {
  return {
    id: asString(get(raw, "id")),
    model: asString(get(raw, "model")),
    system_fingerprint: asString(get(raw, "system_fingerprint")),
    service_tier: asString(get(raw, "service_tier")),
    choices: parseChoices(get(raw, "choices")),
    usage: parseUsage(get(raw, "usage")),
  }
}

export function parseOpenAiChatCall(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
): OpenAiChatCall {
  const req = parseJsonOrNull(requestBody)
  const resp = parseJsonOrNull(responseBody)
  return {
    request: req
      ? parseRequest(req)
      : {
          model: null,
          messages: [],
          tools: [],
          response_format: null,
          sampling: EMPTY_SAMPLING,
        },
    response: resp
      ? parseResponse(resp)
      : {
          id: null,
          model: null,
          system_fingerprint: null,
          service_tier: null,
          choices: [],
          usage: EMPTY_USAGE,
        },
  }
}
