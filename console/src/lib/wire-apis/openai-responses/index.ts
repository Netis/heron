import { asArray, asBoolean, asNumber, asString, asUint, get, parseJsonOrNull } from "../shared"
import type {
  OpenAiResponsesCall,
  ResponsesContentPart,
  ResponsesItem,
  ResponsesMessageItem,
  ResponsesReasoningConfig,
  ResponsesRequest,
  ResponsesResponse,
  ResponsesRole,
  ResponsesSampling,
  ResponsesToolDef,
  ResponsesUsage,
} from "./types"

export type { OpenAiResponsesCall } from "./types"

const EMPTY_SAMPLING: ResponsesSampling = {
  temperature: null,
  max_output_tokens: null,
  top_p: null,
  stream: null,
  tool_choice: undefined,
  parallel_tool_calls: null,
  previous_response_id: null,
  store: null,
  metadata: null,
  truncation: null,
  include: [],
  user: null,
  service_tier: null,
}

const EMPTY_USAGE: ResponsesUsage = {
  input_tokens: null,
  output_tokens: null,
  total_tokens: null,
  cached_input_tokens: null,
  reasoning_tokens: null,
}

// ── content parts ──────────────────────────────────────────────────────────

function parseContentParts(raw: unknown): string | ResponsesContentPart[] {
  const s = asString(raw)
  if (s != null) return s
  const arr = asArray(raw)
  if (!arr) return []
  const out: ResponsesContentPart[] = []
  for (const item of arr) {
    const t = asString(get(item, "type"))
    switch (t) {
      case "input_text":
        out.push({ type: "input_text", text: asString(get(item, "text")) ?? "" })
        break
      case "output_text": {
        const ann = asArray(get(item, "annotations")) ?? undefined
        out.push({
          type: "output_text",
          text: asString(get(item, "text")) ?? "",
          ...(ann ? { annotations: ann } : {}),
        })
        break
      }
      case "text":
        out.push({ type: "text", text: asString(get(item, "text")) ?? "" })
        break
      case "input_image": {
        const detail = asString(get(item, "detail"))
        out.push({
          type: "input_image",
          image_url: asString(get(item, "image_url")) ?? undefined,
          file_id: asString(get(item, "file_id")) ?? undefined,
          detail:
            detail === "auto" || detail === "low" || detail === "high" ? detail : undefined,
        })
        break
      }
      case "input_file":
        out.push({
          type: "input_file",
          file_id: asString(get(item, "file_id")) ?? undefined,
          filename: asString(get(item, "filename")) ?? undefined,
          file_data: asString(get(item, "file_data")) ?? undefined,
        })
        break
      case "refusal":
        out.push({ type: "refusal", refusal: asString(get(item, "refusal")) ?? "" })
        break
      default:
        out.push({ type: "unknown", raw: item })
    }
  }
  return out
}

// ── items ───────────────────────────────────────────────────────────────────

function parseItem(raw: unknown): ResponsesItem {
  const type = asString(get(raw, "type"))
  const role = asString(get(raw, "role"))
  const isMessage = type === "message" || (type == null && role != null)
  if (isMessage) {
    const r = role
    const normalized: ResponsesRole = r === "system" || r === "developer" || r === "user" || r === "assistant" ? r : "assistant"
    const msg: ResponsesMessageItem = {
      kind: "message",
      role: normalized,
      content: parseContentParts(get(raw, "content")),
    }
    const id = asString(get(raw, "id"))
    if (id) msg.id = id
    const status = asString(get(raw, "status"))
    if (status) msg.status = status
    return msg
  }
  switch (type) {
    case "function_call": {
      const out: ResponsesItem = {
        kind: "function_call",
        id: asString(get(raw, "id")) ?? undefined,
        call_id: asString(get(raw, "call_id")) ?? "",
        name: asString(get(raw, "name")) ?? "",
        arguments: asString(get(raw, "arguments")) ?? "",
        status: asString(get(raw, "status")) ?? undefined,
      }
      return out
    }
    case "function_call_output":
      return {
        kind: "function_call_output",
        id: asString(get(raw, "id")) ?? undefined,
        call_id: asString(get(raw, "call_id")) ?? "",
        output: get(raw, "output"),
      }
    case "reasoning": {
      const summaryArr = asArray(get(raw, "summary")) ?? []
      const summary: string[] = []
      for (const s of summaryArr) {
        const t = asString(get(s, "text"))
        if (t != null) summary.push(t)
      }
      return {
        kind: "reasoning",
        id: asString(get(raw, "id")) ?? undefined,
        summary,
        encrypted_content: asString(get(raw, "encrypted_content")) ?? undefined,
        status: asString(get(raw, "status")) ?? undefined,
      }
    }
    case "file_search_call": {
      const queries = asArray(get(raw, "queries"))?.map((q) => asString(q)).filter((s): s is string => s != null)
      const results = asArray(get(raw, "results")) ?? undefined
      return {
        kind: "file_search_call",
        id: asString(get(raw, "id")) ?? undefined,
        queries: queries && queries.length > 0 ? queries : undefined,
        results,
        status: asString(get(raw, "status")) ?? undefined,
      }
    }
    case "web_search_call":
      return {
        kind: "web_search_call",
        id: asString(get(raw, "id")) ?? undefined,
        status: asString(get(raw, "status")) ?? undefined,
        action: get(raw, "action"),
      }
    case "computer_call":
      return {
        kind: "computer_call",
        id: asString(get(raw, "id")) ?? undefined,
        action: get(raw, "action"),
        status: asString(get(raw, "status")) ?? undefined,
      }
    case "mcp_call":
      return {
        kind: "mcp_call",
        id: asString(get(raw, "id")) ?? undefined,
        server_label: asString(get(raw, "server_label")) ?? undefined,
        name: asString(get(raw, "name")) ?? undefined,
        arguments: asString(get(raw, "arguments")) ?? undefined,
        output: get(raw, "output"),
        error: asString(get(raw, "error")) ?? undefined,
        status: asString(get(raw, "status")) ?? undefined,
      }
    default:
      return { kind: "unknown", raw }
  }
}

function parseItems(raw: unknown): ResponsesItem[] {
  // Responses allows `input` to be a plain string — we wrap as a single user message.
  const s = asString(raw)
  if (s != null) {
    return [{ kind: "message", role: "user", content: s }]
  }
  const arr = asArray(raw)
  if (!arr) return []
  return arr.map(parseItem)
}

// ── tools ───────────────────────────────────────────────────────────────────

function parseTools(raw: unknown): ResponsesToolDef[] {
  const arr = asArray(raw)
  if (!arr) return []
  const out: ResponsesToolDef[] = []
  for (const t of arr) {
    const type = asString(get(t, "type")) ?? ""
    const def: ResponsesToolDef = { type, raw: t }
    if (type === "function") {
      def.name = asString(get(t, "name")) ?? undefined
      def.description = asString(get(t, "description")) ?? undefined
      def.parameters = get(t, "parameters")
      def.strict = asBoolean(get(t, "strict")) ?? undefined
    } else if (type === "file_search") {
      const ids = asArray(get(t, "vector_store_ids"))?.map((v) => asString(v)).filter((s): s is string => s != null)
      def.vector_store_ids = ids && ids.length > 0 ? ids : undefined
    } else if (type === "mcp") {
      def.name = asString(get(t, "server_label")) ?? undefined
      def.server_label = asString(get(t, "server_label")) ?? undefined
      def.server_url = asString(get(t, "server_url")) ?? undefined
    }
    out.push(def)
  }
  return out
}

// ── reasoning config ────────────────────────────────────────────────────────

function parseReasoningConfig(raw: unknown): ResponsesReasoningConfig | null {
  if (raw == null) return null
  return {
    effort: asString(get(raw, "effort")),
    summary: asString(get(raw, "summary")),
  }
}

// ── sampling ────────────────────────────────────────────────────────────────

function parseMetadata(v: unknown): Record<string, string> | null {
  if (v == null || typeof v !== "object" || Array.isArray(v)) return null
  const out: Record<string, string> = {}
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    const s = asString(val)
    if (s != null) out[k] = s
  }
  return Object.keys(out).length > 0 ? out : null
}

function parseSampling(body: unknown): ResponsesSampling {
  return {
    temperature: asNumber(get(body, "temperature")),
    max_output_tokens: asUint(get(body, "max_output_tokens")),
    top_p: asNumber(get(body, "top_p")),
    stream: asBoolean(get(body, "stream")),
    tool_choice: get(body, "tool_choice"),
    parallel_tool_calls: asBoolean(get(body, "parallel_tool_calls")),
    previous_response_id: asString(get(body, "previous_response_id")),
    store: asBoolean(get(body, "store")),
    metadata: parseMetadata(get(body, "metadata")),
    truncation: asString(get(body, "truncation")),
    include:
      asArray(get(body, "include"))?.map((v) => asString(v)).filter((s): s is string => s != null) ?? [],
    user: asString(get(body, "user")),
    service_tier: asString(get(body, "service_tier")),
  }
}

// ── request / response ──────────────────────────────────────────────────────

function parseRequest(raw: unknown): ResponsesRequest {
  return {
    model: asString(get(raw, "model")),
    instructions: asString(get(raw, "instructions")),
    input: parseItems(get(raw, "input")),
    tools: parseTools(get(raw, "tools")),
    reasoning: parseReasoningConfig(get(raw, "reasoning")),
    sampling: parseSampling(raw),
  }
}

function parseUsage(raw: unknown): ResponsesUsage {
  if (!raw) return EMPTY_USAGE
  return {
    input_tokens: asUint(get(raw, "input_tokens")),
    output_tokens: asUint(get(raw, "output_tokens")),
    total_tokens: asUint(get(raw, "total_tokens")),
    cached_input_tokens: asUint(get(get(raw, "input_tokens_details"), "cached_tokens")),
    reasoning_tokens: asUint(get(get(raw, "output_tokens_details"), "reasoning_tokens")),
  }
}

function aggregateOutputText(items: ResponsesItem[]): string {
  const parts: string[] = []
  for (const item of items) {
    if (item.kind === "message" && Array.isArray(item.content)) {
      for (const p of item.content) {
        if (p.type === "output_text" || p.type === "text") {
          parts.push(p.text)
        }
      }
    }
  }
  return parts.join("\n")
}

function parseResponse(raw: unknown): ResponsesResponse {
  const output = parseItems(get(raw, "output"))
  return {
    id: asString(get(raw, "id")),
    model: asString(get(raw, "model")),
    status: asString(get(raw, "status")),
    output,
    usage: parseUsage(get(raw, "usage")),
    output_text_aggregated: aggregateOutputText(output),
  }
}

export function parseOpenAiResponsesCall(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
): OpenAiResponsesCall {
  const req = parseJsonOrNull(requestBody)
  const resp = parseJsonOrNull(responseBody)
  return {
    request: req
      ? parseRequest(req)
      : {
          model: null,
          instructions: null,
          input: [],
          tools: [],
          reasoning: null,
          sampling: EMPTY_SAMPLING,
        },
    response: resp
      ? parseResponse(resp)
      : {
          id: null,
          model: null,
          status: null,
          output: [],
          usage: EMPTY_USAGE,
          output_text_aggregated: "",
        },
  }
}
