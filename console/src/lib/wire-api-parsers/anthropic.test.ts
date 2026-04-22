import { describe, expect, it } from "bun:test"
import { readFileSync } from "node:fs"
import path from "node:path"
import { anthropicParser } from "./anthropic"

function fx(name: string): unknown {
  const p = path.resolve(__dirname, "__fixtures__", name)
  return JSON.parse(readFileSync(p, "utf8"))
}

describe("anthropic parseOutput", () => {
  it("text_only → message set, no tool_calls", () => {
    const out = anthropicParser.parseOutput(fx("anthropic_output_text_only.json"))
    expect(out.reasoning).toBeNull()
    expect(out.message?.length ?? 0).toBeGreaterThan(0)
    expect(out.tool_calls).toHaveLength(0)
  })

  it("tool_use → tool_calls populated with name + args_json", () => {
    const out = anthropicParser.parseOutput(fx("anthropic_output_tool_use.json"))
    expect(out.tool_calls).toHaveLength(1)
    const tc = out.tool_calls[0]
    expect(tc.id.startsWith("toolu_")).toBe(true)
    expect(tc.name).toBe("read_file")
    expect(tc.args_json).toContain("\"path\"")
  })

  it("thinking → reasoning + message both set", () => {
    const out = anthropicParser.parseOutput(fx("anthropic_output_thinking.json"))
    expect(out.reasoning?.length ?? 0).toBeGreaterThan(0)
    expect(out.message?.length ?? 0).toBeGreaterThan(0)
  })
})

describe("anthropic parseInput", () => {
  it("user_only → user_message set, no tool_results", () => {
    const out = anthropicParser.parseInput(fx("anthropic_input_user_only.json"))
    expect(out.user_message).not.toBeNull()
    expect(out.tool_results).toHaveLength(0)
  })

  it("with_tool_result → tool_results populated", () => {
    const out = anthropicParser.parseInput(fx("anthropic_input_with_tool_result.json"))
    expect(out.tool_results).toHaveLength(1)
    expect(out.tool_results[0].tool_use_id.startsWith("toolu_")).toBe(true)
    expect(out.tool_results[0].content.length).toBeGreaterThan(0)
  })

  it("full → system extracted", () => {
    const out = anthropicParser.parseInput(fx("anthropic_input_full.json"))
    expect(out.system).toBe("You are a helpful assistant.")
  })

  it("full → messages order + roles with tool-result re-tag", () => {
    const out = anthropicParser.parseInput(fx("anthropic_input_full.json"))
    const roles = out.messages.map((m) => m.role)
    expect(roles).toEqual(["user", "assistant", "tool", "user"])

    const assistant = out.messages[1]
    expect(assistant.content).toHaveLength(2)
    expect(assistant.content[0].type).toBe("text")
    expect(assistant.content[1].type).toBe("tool_use")
    if (assistant.content[1].type === "tool_use") {
      expect(assistant.content[1].name).toBe("read_file")
    }

    const toolMsg = out.messages[2]
    expect(toolMsg.content).toHaveLength(1)
    expect(toolMsg.content[0].type).toBe("tool_result")
    if (toolMsg.content[0].type === "tool_result") {
      expect(toolMsg.content[0].tool_use_id).toBe("toolu_abc")
      expect(toolMsg.content[0].content).toBe("file bytes")
      expect(toolMsg.content[0].is_error).toBe(false)
    }

    const last = out.messages[3]
    expect(last.content).toHaveLength(2)
    expect(last.content[0].type).toBe("image")
    if (last.content[0].type === "image") {
      expect(last.content[0].mime).toBe("image/png")
    }
    expect(last.content[1].type).toBe("text")
  })

  it("full → tools extracted with schema JSON", () => {
    const out = anthropicParser.parseInput(fx("anthropic_input_full.json"))
    expect(out.tools).toHaveLength(2)
    expect(out.tools[0].name).toBe("read_file")
    expect(out.tools[0].description).toBe("Read the contents of a file.")
    const schema = JSON.parse(out.tools[0].input_schema_json)
    expect(schema.type).toBe("object")
  })

  it("full → sampling extracted", () => {
    const out = anthropicParser.parseInput(fx("anthropic_input_full.json"))
    expect(out.sampling.temperature).toBe(0.7)
    expect(out.sampling.top_p).toBe(0.95)
    expect(out.sampling.max_tokens).toBe(8192)
    expect(out.sampling.stream).toBe(true)
    expect(out.sampling.stop).toEqual(["STOP"])
    expect(out.sampling.tool_choice).toBe(`{"type":"auto"}`)
  })

  it("preserves unknown content block types", () => {
    const body = {
      model: "claude-3",
      system: "s",
      messages: [{ role: "user", content: [{ type: "future_kind", foo: 1 }] }],
    }
    const out = anthropicParser.parseInput(body)
    expect(out.messages).toHaveLength(1)
    expect(out.messages[0].content).toHaveLength(1)
    expect(out.messages[0].content[0].type).toBe("unknown")
  })
})
