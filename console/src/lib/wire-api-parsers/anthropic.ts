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
import { asArray, asBoolean, asNumber, asString, asUint, get, toJsonString } from "./shared"

function parseInput(body: unknown): ParsedInput {
  const out = emptyInput()

  out.system = asString(get(body, "system"))

  const tools = asArray(get(body, "tools"))
  if (tools) {
    for (const t of tools) {
      const name = asString(get(t, "name"))
      if (!name) continue
      const def: ParsedToolDef = {
        name,
        description: asString(get(t, "description")),
        input_schema_json: get(t, "input_schema") !== undefined ? toJsonString(get(t, "input_schema")) : "",
      }
      out.tools.push(def)
    }
  }

  out.sampling = {
    temperature: asNumber(get(body, "temperature")),
    max_tokens: asUint(get(body, "max_tokens")),
    top_p: asNumber(get(body, "top_p")),
    top_k: asUint(get(body, "top_k")),
    stream: asBoolean(get(body, "stream")),
    tool_choice: toToolChoice(get(body, "tool_choice")),
    stop: toStringArray(get(body, "stop_sequences")),
    response_format: null,
  }

  const messages = asArray(get(body, "messages"))
  if (!messages) return out

  for (const msg of messages) {
    const wireRole = asString(get(msg, "role"))
    let role: ParsedRole
    if (wireRole === "user") role = "user"
    else if (wireRole === "assistant") role = "assistant"
    else continue

    const blocks: ParsedContentBlock[] = []
    const content = get(msg, "content")

    const contentStr = asString(content)
    if (contentStr != null) {
      blocks.push({ type: "text", text: contentStr })
      if (wireRole === "user") out.user_message = contentStr
    } else {
      const arr = asArray(content)
      if (arr) {
        let userBuf = ""
        for (const block of arr) {
          const blockType = asString(get(block, "type"))
          if (blockType === "text") {
            const text = asString(get(block, "text")) ?? ""
            if (wireRole === "user") {
              if (userBuf.length > 0) userBuf += "\n"
              userBuf += text
            }
            blocks.push({ type: "text", text })
          } else if (blockType === "tool_use") {
            const input = get(block, "input")
            blocks.push({
              type: "tool_use",
              id: asString(get(block, "id")) ?? "",
              name: asString(get(block, "name")) ?? "",
              args_json: input !== undefined ? toJsonString(input) : "",
            })
          } else if (blockType === "tool_result") {
            const toolUseId = asString(get(block, "tool_use_id")) ?? ""
            const isError = asBoolean(get(block, "is_error")) ?? false
            const contentStr = extractToolResultContent(get(block, "content"))
            out.tool_results.push({ tool_use_id: toolUseId, content: contentStr, is_error: isError })
            blocks.push({ type: "tool_result", tool_use_id: toolUseId, content: contentStr, is_error: isError })
          } else if (blockType === "image") {
            const mime = asString(get(get(block, "source"), "media_type"))
            blocks.push({ type: "image", mime, size_bytes: null })
          } else {
            blocks.push({ type: "unknown", raw: block })
          }
        }
        if (wireRole === "user" && userBuf.length > 0) out.user_message = userBuf
      }
    }

    // Re-tag user messages whose content is exclusively tool_result blocks to Tool.
    if (role === "user" && blocks.length > 0 && blocks.every((b) => b.type === "tool_result")) {
      role = "tool"
    }

    const parsedMsg: ParsedMessage = { role, content: blocks }
    out.messages.push(parsedMsg)
  }

  return out
}

function parseOutput(body: unknown): ParsedOutput {
  const out = emptyOutput()
  const content = asArray(get(body, "content"))
  if (!content) return out
  let reasoningBuf = ""
  let messageBuf = ""
  for (const block of content) {
    const blockType = asString(get(block, "type"))
    if (blockType === "thinking") {
      const t = asString(get(block, "thinking"))
      if (t) {
        if (reasoningBuf.length > 0) reasoningBuf += "\n"
        reasoningBuf += t
      }
    } else if (blockType === "text") {
      const t = asString(get(block, "text"))
      if (t) {
        if (messageBuf.length > 0) messageBuf += "\n"
        messageBuf += t
      }
    } else if (blockType === "tool_use") {
      const input = get(block, "input")
      const tc: ParsedToolCall = {
        id: asString(get(block, "id")) ?? "",
        name: asString(get(block, "name")) ?? "",
        args_json: input !== undefined ? toJsonString(input) : "",
      }
      out.tool_calls.push(tc)
    }
  }
  if (reasoningBuf.length > 0) out.reasoning = reasoningBuf
  if (messageBuf.length > 0) out.message = messageBuf
  return out
}

function extractToolResultContent(c: unknown): string {
  if (typeof c === "string") return c
  const arr = asArray(c)
  if (arr) {
    const parts: string[] = []
    for (const b of arr) {
      const t = asString(get(b, "text"))
      if (t != null) parts.push(t)
    }
    return parts.join("\n")
  }
  return c == null ? "" : toJsonString(c)
}

function toToolChoice(v: unknown): string | null {
  if (v === undefined) return null
  const s = asString(v)
  if (s != null) return s
  return toJsonString(v)
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

// Mirrors server/ts-llm/src/wire_apis/anthropic.rs :: parse_input / parse_output — keep in sync.
export const anthropicParser: WireApiParser = {
  parseInput,
  parseOutput,
}
