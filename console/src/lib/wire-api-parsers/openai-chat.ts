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

  const tools = asArray(get(body, "tools"))
  if (tools) {
    for (const t of tools) {
      const f = get(t, "function")
      const name = asString(get(f, "name"))
      if (!name) continue
      const def: ParsedToolDef = {
        name,
        description: asString(get(f, "description")),
        input_schema_json: get(f, "parameters") !== undefined ? toJsonString(get(f, "parameters")) : "",
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

  const messages = asArray(get(body, "messages"))
  if (!messages) return out

  for (const msg of messages) {
    const wireRole = asString(get(msg, "role"))
    let role: ParsedRole
    if (wireRole === "system") role = "system"
    else if (wireRole === "user") role = "user"
    else if (wireRole === "assistant") role = "assistant"
    else if (wireRole === "tool") role = "tool"
    else continue

    let blocks: ParsedContentBlock[] = []

    const contentStr = asString(get(msg, "content"))
    if (contentStr != null) {
      if (contentStr.length > 0) {
        blocks.push({ type: "text", text: contentStr })
      }
      if (wireRole === "user") out.user_message = contentStr
    } else {
      const arr = asArray(get(msg, "content"))
      if (arr) {
        let userBuf = ""
        for (const part of arr) {
          const partType = asString(get(part, "type"))
          if (partType === "text" || partType === "input_text") {
            const text = asString(get(part, "text")) ?? ""
            if (wireRole === "user") {
              if (userBuf.length > 0) userBuf += "\n"
              userBuf += text
            }
            blocks.push({ type: "text", text })
          } else if (partType === "image_url") {
            const url = asString(get(get(part, "image_url"), "url"))
            const mime = extractDataUriMime(url)
            blocks.push({ type: "image", mime, size_bytes: null })
          } else {
            blocks.push({ type: "unknown", raw: part })
          }
        }
        if (wireRole === "user" && userBuf.length > 0) out.user_message = userBuf
      }
    }

    if (wireRole === "assistant") {
      const tcs = asArray(get(msg, "tool_calls"))
      if (tcs) {
        for (const tc of tcs) {
          const f = get(tc, "function")
          blocks.push({
            type: "tool_use",
            id: asString(get(tc, "id")) ?? "",
            name: asString(get(f, "name")) ?? "",
            args_json: asString(get(f, "arguments")) ?? "",
          })
        }
      }
    }

    if (wireRole === "tool") {
      const toolUseId = asString(get(msg, "tool_call_id")) ?? ""
      const contentStr = stringOrJson(get(msg, "content"))
      out.tool_results.push({ tool_use_id: toolUseId, content: contentStr, is_error: false })
      blocks = [{ type: "tool_result", tool_use_id: toolUseId, content: contentStr, is_error: false }]
    }

    const parsedMsg: ParsedMessage = { role, content: blocks }
    out.messages.push(parsedMsg)
  }

  return out
}

function parseOutput(body: unknown): ParsedOutput {
  const out = emptyOutput()
  const choices = asArray(get(body, "choices"))
  const msg = choices && choices.length > 0 ? get(choices[0], "message") : undefined
  if (msg === undefined) return out

  const content = asString(get(msg, "content"))
  if (content && content.length > 0) out.message = content

  const reasoning = asString(get(msg, "reasoning_content"))
  if (reasoning && reasoning.length > 0) out.reasoning = reasoning

  const tcs = asArray(get(msg, "tool_calls"))
  if (tcs) {
    for (const tc of tcs) {
      const f = get(tc, "function")
      const parsed: ParsedToolCall = {
        id: asString(get(tc, "id")) ?? "",
        name: asString(get(f, "name")) ?? "",
        args_json: asString(get(f, "arguments")) ?? "",
      }
      out.tool_calls.push(parsed)
    }
  }
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

// Mirrors server/ts-llm/src/wire_apis/openai.rs :: OpenAiChatWireApi — keep in sync.
export const openaiChatParser: WireApiParser = {
  parseInput,
  parseOutput,
}
