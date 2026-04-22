import { describe, expect, it } from "bun:test"
import { classifyOpenAiResponsesType } from "./classify"

describe("classifyOpenAiResponsesType", () => {
  it("returns 'final' when callId matches finalCallId", () => {
    expect(classifyOpenAiResponsesType(null, "c1", "c1")).toBe("final")
  })

  it("returns 'tool_call' when output has function_call", () => {
    const body = JSON.stringify({
      output: [{ type: "function_call", call_id: "call_1", name: "f", arguments: "{}" }],
    })
    expect(classifyOpenAiResponsesType(body, "c1", null)).toBe("tool_call")
  })

  it("returns 'tool_call' when output has file_search_call / web_search_call / mcp_call", () => {
    for (const t of ["file_search_call", "web_search_call", "mcp_call", "computer_call"]) {
      const body = JSON.stringify({ output: [{ type: t }] })
      expect(classifyOpenAiResponsesType(body, "c1", null)).toBe("tool_call")
    }
  })

  it("returns 'text' for message-only output", () => {
    const body = JSON.stringify({
      output: [{ type: "message", role: "assistant", content: [{ type: "output_text", text: "hi" }] }],
    })
    expect(classifyOpenAiResponsesType(body, "c1", null)).toBe("text")
  })

  it("returns 'text' for reasoning-only output (reasoning is not a tool call)", () => {
    const body = JSON.stringify({
      output: [{ type: "reasoning", summary: [{ type: "summary_text", text: "thinking" }] }],
    })
    expect(classifyOpenAiResponsesType(body, "c1", null)).toBe("text")
  })
})
