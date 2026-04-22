import { asArray, asBoolean, asNumber, asString, asUint, get, parseJsonOrNull } from "../shared"
import type {
  AnthropicBlock,
  AnthropicCacheControl,
  AnthropicCall,
  AnthropicMessage,
  AnthropicRequest,
  AnthropicResponse,
  AnthropicRole,
  AnthropicSampling,
  AnthropicSystem,
  AnthropicTextBlock,
  AnthropicToolDef,
  AnthropicUsage,
} from "./types"

export type { AnthropicCall } from "./types"

const EMPTY_USAGE: AnthropicUsage = {
  input_tokens: null,
  output_tokens: null,
  cache_read_input_tokens: null,
  cache_creation_input_tokens: null,
}

const EMPTY_SAMPLING: AnthropicSampling = {
  max_tokens: null,
  temperature: null,
  top_p: null,
  top_k: null,
  stream: null,
  stop_sequences: [],
  tool_choice: undefined,
  user_id: null,
}

function cacheControlOf(v: unknown): AnthropicCacheControl | undefined {
  const type = asString(get(v, "type"))
  if (type !== "ephemeral") return undefined
  const ttl = asString(get(v, "ttl")) ?? undefined
  return ttl != null ? { type: "ephemeral", ttl } : { type: "ephemeral" }
}

function parseBlock(raw: unknown): AnthropicBlock {
  const type = asString(get(raw, "type"))
  const cc = cacheControlOf(get(raw, "cache_control"))
  switch (type) {
    case "text":
      return {
        type: "text",
        text: asString(get(raw, "text")) ?? "",
        ...(cc ? { cache_control: cc } : {}),
      }
    case "tool_use":
      return {
        type: "tool_use",
        id: asString(get(raw, "id")) ?? "",
        name: asString(get(raw, "name")) ?? "",
        input: get(raw, "input"),
        ...(cc ? { cache_control: cc } : {}),
      }
    case "tool_result": {
      const content = get(raw, "content")
      const s = asString(content)
      return {
        type: "tool_result",
        tool_use_id: asString(get(raw, "tool_use_id")) ?? "",
        content: s != null ? s : asArray(content) != null ? (content as Array<{ type: string; [k: string]: unknown }>) : "",
        is_error: asBoolean(get(raw, "is_error")) ?? false,
        ...(cc ? { cache_control: cc } : {}),
      }
    }
    case "image": {
      const source = get(raw, "source")
      const sourceType = asString(get(source, "type"))
      if (sourceType === "base64") {
        return {
          type: "image",
          source: {
            type: "base64",
            media_type: asString(get(source, "media_type")) ?? "",
            data: asString(get(source, "data")) ?? "",
          },
          ...(cc ? { cache_control: cc } : {}),
        }
      }
      if (sourceType === "url") {
        return {
          type: "image",
          source: { type: "url", url: asString(get(source, "url")) ?? "" },
          ...(cc ? { cache_control: cc } : {}),
        }
      }
      return { type: "unknown", raw }
    }
    case "document":
      return {
        type: "document",
        source: get(raw, "source"),
        title: asString(get(raw, "title")) ?? undefined,
        context: asString(get(raw, "context")) ?? undefined,
        ...(cc ? { cache_control: cc } : {}),
      }
    case "thinking":
      return {
        type: "thinking",
        thinking: asString(get(raw, "thinking")) ?? "",
        signature: asString(get(raw, "signature")) ?? undefined,
      }
    case "redacted_thinking":
      return {
        type: "redacted_thinking",
        data: asString(get(raw, "data")) ?? "",
      }
    default:
      return { type: "unknown", raw }
  }
}

function parseSystem(v: unknown): AnthropicSystem | null {
  const s = asString(v)
  if (s != null) return { kind: "string", text: s }
  const arr = asArray(v)
  if (arr) {
    const blocks: AnthropicTextBlock[] = []
    for (const item of arr) {
      if (asString(get(item, "type")) === "text") {
        const cc = cacheControlOf(get(item, "cache_control"))
        blocks.push({
          type: "text",
          text: asString(get(item, "text")) ?? "",
          ...(cc ? { cache_control: cc } : {}),
        })
      }
    }
    return { kind: "blocks", blocks }
  }
  return null
}

function parseTools(v: unknown): AnthropicToolDef[] {
  const arr = asArray(v)
  if (!arr) return []
  const out: AnthropicToolDef[] = []
  for (const t of arr) {
    const name = asString(get(t, "name"))
    if (!name) continue
    const cc = cacheControlOf(get(t, "cache_control"))
    out.push({
      name,
      description: asString(get(t, "description")),
      input_schema: get(t, "input_schema"),
      ...(cc ? { cache_control: cc } : {}),
    })
  }
  return out
}

function parseSampling(body: unknown): AnthropicSampling {
  return {
    max_tokens: asUint(get(body, "max_tokens")),
    temperature: asNumber(get(body, "temperature")),
    top_p: asNumber(get(body, "top_p")),
    top_k: asUint(get(body, "top_k")),
    stream: asBoolean(get(body, "stream")),
    stop_sequences: toStringArray(get(body, "stop_sequences")),
    tool_choice: get(body, "tool_choice"),
    user_id: asString(get(get(body, "metadata"), "user_id")),
  }
}

function toStringArray(v: unknown): string[] {
  const arr = asArray(v)
  if (!arr) return []
  const out: string[] = []
  for (const x of arr) {
    const s = asString(x)
    if (s != null) out.push(s)
  }
  return out
}

function countCacheControlMarkers(
  system: AnthropicSystem | null,
  messages: AnthropicMessage[],
  tools: AnthropicToolDef[],
): number {
  let n = 0
  if (system?.kind === "blocks") {
    for (const b of system.blocks) if (b.cache_control) n++
  }
  for (const m of messages) {
    for (const b of m.content) {
      if ("cache_control" in b && (b as { cache_control?: unknown }).cache_control) n++
    }
  }
  for (const t of tools) if (t.cache_control) n++
  return n
}

function parseRequest(raw: unknown): AnthropicRequest {
  const messagesRaw = asArray(get(raw, "messages"))
  const messages: AnthropicMessage[] = []
  if (messagesRaw) {
    for (const msg of messagesRaw) {
      const role = asString(get(msg, "role"))
      if (role !== "user" && role !== "assistant") continue
      const content = get(msg, "content")
      const contentStr = asString(content)
      const blocks: AnthropicBlock[] = []
      if (contentStr != null) {
        blocks.push({ type: "text", text: contentStr })
      } else {
        const arr = asArray(content)
        if (arr) {
          for (const b of arr) blocks.push(parseBlock(b))
        }
      }
      messages.push({ role: role as AnthropicRole, content: blocks })
    }
  }

  const system = parseSystem(get(raw, "system"))
  const tools = parseTools(get(raw, "tools"))
  const sampling = parseSampling(raw)
  const cache_control_count = countCacheControlMarkers(system, messages, tools)

  return {
    model: asString(get(raw, "model")),
    system,
    messages,
    tools,
    sampling,
    cache_control_count,
  }
}

function parseResponse(raw: unknown): AnthropicResponse {
  const contentArr = asArray(get(raw, "content"))
  const content: AnthropicBlock[] = contentArr ? contentArr.map(parseBlock) : []
  const usageRaw = get(raw, "usage")
  const usage: AnthropicUsage = usageRaw
    ? {
        input_tokens: asUint(get(usageRaw, "input_tokens")),
        output_tokens: asUint(get(usageRaw, "output_tokens")),
        cache_read_input_tokens: asUint(get(usageRaw, "cache_read_input_tokens")),
        cache_creation_input_tokens: asUint(get(usageRaw, "cache_creation_input_tokens")),
      }
    : EMPTY_USAGE
  return {
    id: asString(get(raw, "id")),
    model: asString(get(raw, "model")),
    role: asString(get(raw, "role")),
    content,
    stop_reason: asString(get(raw, "stop_reason")),
    stop_sequence: asString(get(raw, "stop_sequence")),
    usage,
  }
}

export function parseAnthropicCall(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
): AnthropicCall {
  const req = parseJsonOrNull(requestBody)
  const resp = parseJsonOrNull(responseBody)
  return {
    request: req
      ? parseRequest(req)
      : {
          model: null,
          system: null,
          messages: [],
          tools: [],
          sampling: EMPTY_SAMPLING,
          cache_control_count: 0,
        },
    response: resp
      ? parseResponse(resp)
      : {
          id: null,
          model: null,
          role: null,
          content: [],
          stop_reason: null,
          stop_sequence: null,
          usage: EMPTY_USAGE,
        },
  }
}
