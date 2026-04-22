import { describe, expect, it } from "bun:test"
import { deriveCallPreview } from "./preview"

describe("deriveCallPreview", () => {
  it("returns 'final' when callId matches finalCallId", () => {
    const p = deriveCallPreview("anthropic", null, "c1", "c1")
    expect(p.type).toBe("final")
    expect(p.toolCalls).toHaveLength(0)
  })

  it("returns 'tool_call' when response has tool_use blocks (anthropic)", () => {
    const body = JSON.stringify({
      content: [
        { type: "text", text: "let me check" },
        { type: "tool_use", id: "toolu_1", name: "read_file", input: { path: "x" } },
      ],
      stop_reason: "tool_use",
    })
    const p = deriveCallPreview("anthropic", body, "c1", "cFinal")
    expect(p.type).toBe("tool_call")
    expect(p.toolCalls).toHaveLength(1)
    expect(p.toolCalls[0].name).toBe("read_file")
  })

  it("returns 'text' with messagePreview when response has text only (anthropic)", () => {
    const body = JSON.stringify({
      content: [{ type: "text", text: "Hello, this is a plain reply." }],
      stop_reason: "end_turn",
    })
    const p = deriveCallPreview("anthropic", body, "c1", "cFinal")
    expect(p.type).toBe("text")
    expect(p.messagePreview).toBe("Hello, this is a plain reply.")
    expect(p.hasReasoning).toBe(false)
  })

  it("truncates messagePreview at 60 chars", () => {
    const long = "x".repeat(200)
    const body = JSON.stringify({ content: [{ type: "text", text: long }] })
    const p = deriveCallPreview("anthropic", body, "c1", null)
    expect(p.messagePreview?.length).toBe(60)
  })

  it("unknown wire_api falls back to text with empty preview", () => {
    const p = deriveCallPreview("unknown", JSON.stringify({ foo: 1 }), "c1", null)
    expect(p.type).toBe("text")
    expect(p.messagePreview).toBeNull()
  })
})
