import type {
  ParsedContentBlock,
  ParsedInput,
  ParsedMessage,
  ParsedOutput,
  ParsedRole,
  ParsedToolCall,
  ParsedToolDef,
  WireApiParser,
} from "./types"
import { emptyInput, emptyOutput } from "./types"
import { asArray, asBoolean, asNumber, asString, asUint, get, stringOrJson, toJsonString } from "./shared"

function parseInput(body: unknown): ParsedInput {
  const out = emptyInput()

  out.system = asString(get(body, "instructions"))

  const tools = asArray(get(body, "tools"))
  if (tools) {
    for (const t of tools) {
      const name = asString(get(t, "name"))
      if (!name) continue
      const def: ParsedToolDef = {
        name,
        description: asString(get(t, "description")),
        input_schema_json: get(t, "parameters") !== undefined ? toJsonString(get(t, "parameters")) : "",
      }
      out.tools.push(def)
    }
  }

  const maxTokens = asUint(get(body, "max_completion_tokens")) ?? asUint(get(body, "max_tokens"))
  out.sampling = {
    temperature: asNumber(get(body, "temperature")),
    max_tokens: maxTokens,
    top_p: asNumber(get(body, "top_p")),
    top_k: null,
    stream: asBoolean(get(body, "stream")),
    tool_choice: toToolChoice(get(body, "tool_choice")),
    stop: toStopArray(get(body, "stop")),
    response_format: get(body, "response_format") !== undefined ? toJsonString(get(body, "response_format")) : null,
  }

  const inputField = get(body, "input")
  const inputStr = asString(inputField)
  if (inputStr != null) {
    out.user_message = inputStr
    const msg: ParsedMessage = { role: "user", content: [{ type: "text", text: inputStr }] }
    out.messages.push(msg)
    return out
  }

  const items = asArray(inputField)
  if (!items) return out

  for (const item of items) {
    const typeField = asString(get(item, "type"))
    const hasRole = get(item, "role") !== undefined
    const isMessage = typeField === "message" || (typeField == null && hasRole)

    if (isMessage) {
      const wireRole = asString(get(item, "role"))
      let role: ParsedRole
      if (wireRole === "system") role = "system"
      else if (wireRole === "user") role = "user"
      else if (wireRole === "assistant") role = "assistant"
      else continue

      const blocks: ParsedContentBlock[] = []
      let userBuf = ""
      const contentStr = asString(get(item, "content"))
      if (contentStr != null) {
        if (wireRole === "user") out.user_message = contentStr
        if (contentStr.length > 0) blocks.push({ type: "text", text: contentStr })
      } else {
        const arr = asArray(get(item, "content"))
        if (arr) {
          for (const part of arr) {
            const partType = asString(get(part, "type"))
            if (partType === "input_text" || partType === "text" || partType === "output_text") {
              const text = asString(get(part, "text")) ?? ""
              if (wireRole === "user") {
                if (userBuf.length > 0) userBuf += "\n"
                userBuf += text
              }
              blocks.push({ type: "text", text })
            } else if (partType === "input_image") {
              const url = asString(get(part, "image_url"))
              blocks.push({ type: "image", mime: extractDataUriMime(url), size_bytes: null })
            } else {
              blocks.push({ type: "unknown", raw: part })
            }
          }
        }
      }
      if (wireRole === "user" && userBuf.length > 0) out.user_message = userBuf
      const msg: ParsedMessage = { role, content: blocks }
      out.messages.push(msg)
      continue
    }

    if (typeField === "function_call") {
      const tc: ParsedContentBlock = {
        type: "tool_use",
        id: asString(get(item, "call_id")) ?? "",
        name: asString(get(item, "name")) ?? "",
        args_json: asString(get(item, "arguments")) ?? "",
      }
      const msg: ParsedMessage = { role: "assistant", content: [tc] }
      out.messages.push(msg)
    } else if (typeField === "function_call_output") {
      const toolUseId = asString(get(item, "call_id")) ?? ""
      const contentStr = stringOrJson(get(item, "output"))
      out.tool_results.push({ tool_use_id: toolUseId, content: contentStr, is_error: false })
      const msg: ParsedMessage = {
        role: "tool",
        content: [{ type: "tool_result", tool_use_id: toolUseId, content: contentStr, is_error: false }],
      }
      out.messages.push(msg)
    } else {
      // Unknown non-message item — attribute to assistant (typically model-emitted).
      const msg: ParsedMessage = {
        role: "assistant",
        content: [{ type: "unknown", raw: item }],
      }
      out.messages.push(msg)
    }
  }

  return out
}

function parseOutput(body: unknown): ParsedOutput {
  const out = emptyOutput()
  const items = asArray(get(body, "output"))
  if (!items) return out
  let reasoningBuf = ""
  let messageBuf = ""
  for (const item of items) {
    const typeField = asString(get(item, "type"))
    if (typeField === "reasoning") {
      const summary = asArray(get(item, "summary"))
      if (summary) {
        for (const s of summary) {
          const t = asString(get(s, "text"))
          if (t) {
            if (reasoningBuf.length > 0) reasoningBuf += "\n"
            reasoningBuf += t
          }
        }
      }
    } else if (typeField === "message") {
      const content = asArray(get(item, "content"))
      if (content) {
        for (const part of content) {
          if (asString(get(part, "type")) === "output_text") {
            const t = asString(get(part, "text"))
            if (t) {
              if (messageBuf.length > 0) messageBuf += "\n"
              messageBuf += t
            }
          }
        }
      }
    } else if (typeField === "function_call") {
      const tc: ParsedToolCall = {
        id: asString(get(item, "call_id")) ?? "",
        name: asString(get(item, "name")) ?? "",
        args_json: asString(get(item, "arguments")) ?? "",
      }
      out.tool_calls.push(tc)
    }
  }
  if (reasoningBuf.length > 0) out.reasoning = reasoningBuf
  if (messageBuf.length > 0) out.message = messageBuf
  return out
}

function extractDataUriMime(url: string | null): string | null {
  if (!url) return null
  if (!url.startsWith("data:")) return null
  const rest = url.slice(5)
  const semi = rest.indexOf(";")
  return semi === -1 ? rest : rest.slice(0, semi)
}

function toToolChoice(v: unknown): string | null {
  if (v === undefined) return null
  const s = asString(v)
  if (s != null) return s
  return toJsonString(v)
}

function toStopArray(v: unknown): string[] {
  const s = asString(v)
  if (s != null) return [s]
  const arr = asArray(v)
  if (!arr) return []
  const out: string[] = []
  for (const x of arr) {
    const s = asString(x)
    if (s != null) out.push(s)
  }
  return out
}

// Mirrors server/ts-llm/src/wire_apis/openai.rs :: OpenAiResponsesWireApi — keep in sync.
export const openaiResponsesParser: WireApiParser = {
  parseInput,
  parseOutput,
}
