import { describe, expect, it } from "bun:test"
import { classifyAnthropicType } from "./classify"

describe("classifyAnthropicType", () => {
  it("returns 'final' when callId matches finalCallId regardless of content", () => {
    expect(classifyAnthropicType(null, "c1", "c1")).toBe("final")
  })

  it("returns 'tool_call' when response has a tool_use block", () => {
    const body = JSON.stringify({
      content: [
        { type: "text", text: "let me check" },
        { type: "tool_use", id: "toolu_1", name: "read", input: {} },
      ],
    })
    expect(classifyAnthropicType(body, "c1", "cFinal")).toBe("tool_call")
  })

  it("returns 'text' when response only has text blocks", () => {
    const body = JSON.stringify({ content: [{ type: "text", text: "hello" }] })
    expect(classifyAnthropicType(body, "c1", null)).toBe("text")
  })

  it("returns 'text' for unparseable / null response body", () => {
    expect(classifyAnthropicType(null, "c1", null)).toBe("text")
    expect(classifyAnthropicType("not-json", "c1", null)).toBe("text")
  })

  it("returns 'text' even if there's a thinking block but no tool_use", () => {
    const body = JSON.stringify({
      content: [
        { type: "thinking", thinking: "..." },
        { type: "text", text: "final" },
      ],
    })
    expect(classifyAnthropicType(body, "c1", null)).toBe("text")
  })
})
