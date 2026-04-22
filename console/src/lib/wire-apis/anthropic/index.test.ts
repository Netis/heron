import { describe, expect, it } from "bun:test"
import { readFileSync } from "node:fs"
import path from "node:path"
import { parseAnthropicCall } from "./index"

function fx(name: string): string {
  return readFileSync(path.resolve(__dirname, "__fixtures__", name), "utf8")
}

describe("parseAnthropicCall — request", () => {
  it("full fixture: system string extracted", () => {
    const call = parseAnthropicCall(fx("anthropic_input_full.json"), null)
    expect(call.request.system).toEqual({ kind: "string", text: "You are a helpful assistant." })
  })

  it("full fixture: 4 messages with roles user/assistant (Anthropic keeps wire roles)", () => {
    const call = parseAnthropicCall(fx("anthropic_input_full.json"), null)
    const roles = call.request.messages.map((m) => m.role)
    // Anthropic native: tool_result-only messages stay as role=user (wire form);
    // no re-tagging to "tool" since Anthropic spec doesn't have that role.
    expect(roles).toEqual(["user", "assistant", "user", "user"])
  })

  it("full fixture: assistant message has text + tool_use blocks", () => {
    const call = parseAnthropicCall(fx("anthropic_input_full.json"), null)
    const assistant = call.request.messages[1]
    expect(assistant.content).toHaveLength(2)
    expect(assistant.content[0].type).toBe("text")
    expect(assistant.content[1].type).toBe("tool_use")
    if (assistant.content[1].type === "tool_use") {
      expect(assistant.content[1].name).toBe("read_file")
      // input preserved as object
      expect(assistant.content[1].input).toEqual({ path: "foo.txt" })
    }
  })

  it("full fixture: tool_result block preserves tool_use_id and content", () => {
    const call = parseAnthropicCall(fx("anthropic_input_full.json"), null)
    const third = call.request.messages[2]
    expect(third.content[0].type).toBe("tool_result")
    if (third.content[0].type === "tool_result") {
      expect(third.content[0].tool_use_id).toBe("toolu_abc")
      expect(third.content[0].content).toBe("file bytes")
      expect(third.content[0].is_error).toBe(false)
    }
  })

  it("full fixture: last user message has image (base64) + text", () => {
    const call = parseAnthropicCall(fx("anthropic_input_full.json"), null)
    const last = call.request.messages[3]
    expect(last.content).toHaveLength(2)
    expect(last.content[0].type).toBe("image")
    if (last.content[0].type === "image" && last.content[0].source.type === "base64") {
      expect(last.content[0].source.media_type).toBe("image/png")
    }
  })

  it("full fixture: tools extracted with native input_schema object", () => {
    const call = parseAnthropicCall(fx("anthropic_input_full.json"), null)
    expect(call.request.tools).toHaveLength(2)
    expect(call.request.tools[0].name).toBe("read_file")
    expect(call.request.tools[0].description).toBe("Read the contents of a file.")
    expect(call.request.tools[0].input_schema).toEqual({
      type: "object",
      properties: { path: { type: "string" } },
    })
  })

  it("full fixture: sampling preserves Anthropic-native fields (stop_sequences, tool_choice object)", () => {
    const call = parseAnthropicCall(fx("anthropic_input_full.json"), null)
    expect(call.request.sampling.max_tokens).toBe(8192)
    expect(call.request.sampling.temperature).toBe(0.7)
    expect(call.request.sampling.top_p).toBe(0.95)
    expect(call.request.sampling.stream).toBe(true)
    expect(call.request.sampling.stop_sequences).toEqual(["STOP"])
    expect(call.request.sampling.tool_choice).toEqual({ type: "auto" })
  })

  it("preserves unknown content block types as type:unknown with raw", () => {
    const body = JSON.stringify({
      model: "claude-3",
      messages: [{ role: "user", content: [{ type: "future_kind", foo: 1 }] }],
    })
    const call = parseAnthropicCall(body, null)
    expect(call.request.messages[0].content[0].type).toBe("unknown")
    if (call.request.messages[0].content[0].type === "unknown") {
      expect(call.request.messages[0].content[0].raw).toEqual({ type: "future_kind", foo: 1 })
    }
  })

  it("system as array of text blocks with cache_control is parsed into blocks form", () => {
    const body = JSON.stringify({
      model: "claude-3",
      system: [
        { type: "text", text: "segment 1" },
        { type: "text", text: "segment 2", cache_control: { type: "ephemeral" } },
      ],
      messages: [{ role: "user", content: "hi" }],
    })
    const call = parseAnthropicCall(body, null)
    expect(call.request.system?.kind).toBe("blocks")
    if (call.request.system?.kind === "blocks") {
      expect(call.request.system.blocks).toHaveLength(2)
      expect(call.request.system.blocks[1].cache_control?.type).toBe("ephemeral")
    }
    // cache_control_count counts the one cache_control marker
    expect(call.request.cache_control_count).toBe(1)
  })
})

describe("parseAnthropicCall — response", () => {
  it("text_only: content has text blocks, no tool_use", () => {
    const call = parseAnthropicCall(null, fx("anthropic_output_text_only.json"))
    expect(call.response.content.some((b) => b.type === "text")).toBe(true)
    expect(call.response.content.every((b) => b.type !== "tool_use")).toBe(true)
  })

  it("tool_use: response has a tool_use block with id, name, input", () => {
    const call = parseAnthropicCall(null, fx("anthropic_output_tool_use.json"))
    const toolUse = call.response.content.find((b) => b.type === "tool_use")
    expect(toolUse).toBeDefined()
    if (toolUse?.type === "tool_use") {
      expect(toolUse.id.startsWith("toolu_")).toBe(true)
      expect(toolUse.name).toBe("read_file")
      expect(typeof toolUse.input).toBe("object")
    }
  })

  it("thinking: response has both thinking block and text block", () => {
    const call = parseAnthropicCall(null, fx("anthropic_output_thinking.json"))
    const thinking = call.response.content.find((b) => b.type === "thinking")
    expect(thinking).toBeDefined()
    if (thinking?.type === "thinking") {
      expect(thinking.thinking.length).toBeGreaterThan(0)
    }
    expect(call.response.content.some((b) => b.type === "text")).toBe(true)
  })

  it("usage fields extracted", () => {
    const body = JSON.stringify({
      content: [{ type: "text", text: "hi" }],
      usage: {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_input_tokens: 80,
        cache_creation_input_tokens: 5,
      },
    })
    const call = parseAnthropicCall(null, body)
    expect(call.response.usage.input_tokens).toBe(100)
    expect(call.response.usage.output_tokens).toBe(20)
    expect(call.response.usage.cache_read_input_tokens).toBe(80)
    expect(call.response.usage.cache_creation_input_tokens).toBe(5)
  })
})
