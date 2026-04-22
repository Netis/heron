import { describe, expect, it } from "bun:test"
import { classifyOpenAiChatType } from "./classify"

describe("classifyOpenAiChatType", () => {
  it("returns 'final' when callId matches finalCallId", () => {
    expect(classifyOpenAiChatType(null, "c1", "c1")).toBe("final")
  })

  it("returns 'tool_call' when message has tool_calls", () => {
    const body = JSON.stringify({
      choices: [
        {
          index: 0,
          message: {
            role: "assistant",
            content: null,
            tool_calls: [{ id: "call_1", type: "function", function: { name: "f", arguments: "{}" } }],
          },
          finish_reason: "tool_calls",
        },
      ],
    })
    expect(classifyOpenAiChatType(body, "c1", null)).toBe("tool_call")
  })

  it("returns 'text' for plain text response", () => {
    const body = JSON.stringify({
      choices: [{ index: 0, message: { role: "assistant", content: "hi" }, finish_reason: "stop" }],
    })
    expect(classifyOpenAiChatType(body, "c1", null)).toBe("text")
  })

  it("returns 'text' for null/invalid body", () => {
    expect(classifyOpenAiChatType(null, "c1", null)).toBe("text")
    expect(classifyOpenAiChatType("garbage", "c1", null)).toBe("text")
  })
})
