import { describe, it, expect } from "vitest"
import { canonicalizeToolId } from "./tool-id"

describe("canonicalizeToolId", () => {
  it("passes through ids that already have the underscore", () => {
    expect(canonicalizeToolId("call_abc")).toBe("call_abc")
    expect(canonicalizeToolId("toolu_xyz")).toBe("toolu_xyz")
    expect(canonicalizeToolId("fc_123")).toBe("fc_123")
    expect(canonicalizeToolId("chatcmpl_xyz")).toBe("chatcmpl_xyz")
  })

  it("inserts the missing underscore for known prefixes", () => {
    expect(canonicalizeToolId("calld9c1e9e6617a41ca860562a1")).toBe("call_d9c1e9e6617a41ca860562a1")
    expect(canonicalizeToolId("tooluxyz")).toBe("toolu_xyz")
    expect(canonicalizeToolId("fcabc")).toBe("fc_abc")
    expect(canonicalizeToolId("chatcmplabc")).toBe("chatcmpl_abc")
  })

  it("passes through unknown prefixes", () => {
    expect(canonicalizeToolId("abc_xyz")).toBe("abc_xyz")
    expect(canonicalizeToolId("custom123")).toBe("custom123")
  })

  it("passes through degenerate prefix-only ids", () => {
    expect(canonicalizeToolId("call")).toBe("call")
    expect(canonicalizeToolId("toolu")).toBe("toolu")
  })
})
