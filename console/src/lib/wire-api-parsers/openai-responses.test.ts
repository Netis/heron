import { describe, expect, it } from "bun:test"
import { readFileSync } from "node:fs"
import path from "node:path"
import { openaiResponsesParser } from "./openai-responses"

function fx(name: string): unknown {
  const p = path.resolve(__dirname, "__fixtures__", name)
  return JSON.parse(readFileSync(p, "utf8"))
}

describe("openai-responses parseOutput", () => {
  it("message → output message text", () => {
    const out = openaiResponsesParser.parseOutput(fx("openai_responses_output_message.json"))
    expect(out.message?.length ?? 0).toBeGreaterThan(0)
    expect(out.tool_calls).toHaveLength(0)
  })

  it("function_call with reasoning → reasoning + tool_calls", () => {
    const out = openaiResponsesParser.parseOutput(fx("openai_responses_output_function_call.json"))
    expect(out.reasoning).not.toBeNull()
    expect(out.tool_calls).toHaveLength(1)
  })
})

describe("openai-responses parseInput", () => {
  it("function_call_output → tool_results populated", () => {
    const out = openaiResponsesParser.parseInput(fx("openai_responses_input_with_function_call_output.json"))
    expect(out.tool_results).toHaveLength(1)
  })

  it("full → instructions become system", () => {
    const out = openaiResponsesParser.parseInput(fx("openai_responses_input_full.json"))
    expect(out.system).toBe("You are a code assistant.")
  })

  it("input as string → single user message", () => {
    const out = openaiResponsesParser.parseInput({ model: "gpt-5", input: "hi" })
    expect(out.messages).toHaveLength(1)
    expect(out.messages[0].role).toBe("user")
    expect(out.messages[0].content[0].type).toBe("text")
    if (out.messages[0].content[0].type === "text") {
      expect(out.messages[0].content[0].text).toBe("hi")
    }
  })

  it("full → items roles + tool_use + tool_result blocks", () => {
    const out = openaiResponsesParser.parseInput(fx("openai_responses_input_full.json"))
    const roles = out.messages.map((m) => m.role)
    expect(roles).toEqual(["user", "assistant", "tool"])

    const assistant = out.messages[1]
    expect(assistant.content[0].type).toBe("tool_use")
    if (assistant.content[0].type === "tool_use") {
      expect(assistant.content[0].id).toBe("call_abc")
      expect(assistant.content[0].name).toBe("run_shell")
      expect(assistant.content[0].args_json).toBe(`{"cmd":"ls"}`)
    }

    const tool = out.messages[2]
    expect(tool.content[0].type).toBe("tool_result")
    if (tool.content[0].type === "tool_result") {
      expect(tool.content[0].tool_use_id).toBe("call_abc")
      expect(tool.content[0].content).toBe("a.txt\nb.txt")
    }
  })

  it("full → tools + sampling extracted", () => {
    const out = openaiResponsesParser.parseInput(fx("openai_responses_input_full.json"))
    expect(out.tools).toHaveLength(1)
    expect(out.tools[0].name).toBe("run_shell")
    expect(out.sampling.temperature).toBe(0.2)
    expect(out.sampling.max_tokens).toBe(4096)
    expect(out.sampling.top_p).toBe(1.0)
    expect(out.sampling.stream).toBe(true)
    expect(out.sampling.tool_choice).toBe("auto")
  })

  it("typeless message item with role + content is treated as a message", () => {
    const out = openaiResponsesParser.parseInput(fx("openai_responses_input_image.json"))
    expect(out.messages).toHaveLength(1)
    expect(out.messages[0].role).toBe("user")
    expect(out.messages[0].content).toHaveLength(2)
    expect(out.messages[0].content[0].type).toBe("text")
    expect(out.messages[0].content[1].type).toBe("image")
  })

  it("function_call_output with non-string output is JSON-stringified", () => {
    const body = {
      model: "gpt-5",
      input: [
        {
          type: "function_call_output",
          call_id: "call_x",
          output: { status: "ok", data: [1, 2, 3] },
        },
      ],
    }
    const out = openaiResponsesParser.parseInput(body)
    expect(out.messages).toHaveLength(1)
    expect(out.messages[0].role).toBe("tool")
    const block = out.messages[0].content[0]
    expect(block.type).toBe("tool_result")
    if (block.type === "tool_result") {
      const parsed = JSON.parse(block.content) as { status: string; data: number[] }
      expect(parsed.status).toBe("ok")
      expect(parsed.data[0]).toBe(1)
    }
    expect(out.tool_results).toHaveLength(1)
    expect(out.tool_results[0].tool_use_id).toBe("call_x")
  })

  it("system-role message item → role=system, top-level system stays null", () => {
    const body = {
      model: "gpt-5",
      input: [
        { type: "message", role: "system", content: [{ type: "text", text: "be brief" }] },
      ],
    }
    const out = openaiResponsesParser.parseInput(body)
    expect(out.messages).toHaveLength(1)
    expect(out.messages[0].role).toBe("system")
    expect(out.messages[0].content[0].type).toBe("text")
    if (out.messages[0].content[0].type === "text") {
      expect(out.messages[0].content[0].text).toBe("be brief")
    }
    expect(out.system).toBeNull()
  })
})
