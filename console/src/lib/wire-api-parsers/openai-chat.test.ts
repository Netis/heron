import { describe, expect, it } from "bun:test"
import { readFileSync } from "node:fs"
import path from "node:path"
import { openaiChatParser } from "./openai-chat"

function fx(name: string): unknown {
  const p = path.resolve(__dirname, "__fixtures__", name)
  return JSON.parse(readFileSync(p, "utf8"))
}

describe("openai-chat parseOutput", () => {
  it("text → message set, no tool_calls", () => {
    const out = openaiChatParser.parseOutput(fx("openai_chat_output_text.json"))
    expect(out.message?.length ?? 0).toBeGreaterThan(0)
    expect(out.tool_calls).toHaveLength(0)
  })

  it("tool_calls → tool_calls[0].id starts with 'call_'", () => {
    const out = openaiChatParser.parseOutput(fx("openai_chat_output_tool_calls.json"))
    expect(out.tool_calls).toHaveLength(1)
    expect(out.tool_calls[0].id.startsWith("call_")).toBe(true)
  })
})

describe("openai-chat parseInput", () => {
  it("tool result → tool_results populated with call_id", () => {
    const out = openaiChatParser.parseInput(fx("openai_chat_input_with_tool_result.json"))
    expect(out.tool_results).toHaveLength(1)
    expect(out.tool_results[0].tool_use_id.startsWith("call_")).toBe(true)
  })

  it("full → messages roles + assistant ToolUse + Tool ToolResult", () => {
    const out = openaiChatParser.parseInput(fx("openai_chat_input_full.json"))
    const roles = out.messages.map((m) => m.role)
    expect(roles).toEqual(["system", "user", "assistant", "tool", "assistant"])

    const assistant = out.messages[2]
    expect(assistant.content).toHaveLength(1)
    expect(assistant.content[0].type).toBe("tool_use")
    if (assistant.content[0].type === "tool_use") {
      expect(assistant.content[0].name).toBe("get_weather")
      expect(assistant.content[0].args_json).toBe(`{"city":"SF"}`)
    }

    const tool = out.messages[3]
    expect(tool.content).toHaveLength(1)
    expect(tool.content[0].type).toBe("tool_result")
    if (tool.content[0].type === "tool_result") {
      expect(tool.content[0].tool_use_id).toBe("call_1")
      expect(tool.content[0].content).toBe("72F sunny")
    }
  })

  it("full → no top-level system (OpenAI Chat uses role=system message)", () => {
    const out = openaiChatParser.parseInput(fx("openai_chat_input_full.json"))
    expect(out.system).toBeNull()
    expect(out.messages[0]?.role).toBe("system")
  })

  it("full → tools extracted", () => {
    const out = openaiChatParser.parseInput(fx("openai_chat_input_full.json"))
    expect(out.tools).toHaveLength(1)
    expect(out.tools[0].name).toBe("get_weather")
    expect(out.tools[0].description).toBe("Get weather for a city.")
    const schema = JSON.parse(out.tools[0].input_schema_json)
    expect(schema.type).toBe("object")
  })

  it("full → sampling extracted", () => {
    const out = openaiChatParser.parseInput(fx("openai_chat_input_full.json"))
    expect(out.sampling.temperature).toBe(0.3)
    expect(out.sampling.max_tokens).toBe(2048)
    expect(out.sampling.top_p).toBe(1.0)
    expect(out.sampling.stream).toBe(true)
    expect(out.sampling.stop).toEqual(["\n\n"])
    expect(out.sampling.tool_choice).toBe("auto")
    expect(out.sampling.response_format).toBe(`{"type":"json_object"}`)
  })
})
