import { describe, expect, it } from "bun:test"
import { joinToolResults } from "./join"

describe("joinToolResults", () => {
  it("joins matching tool_use_id to tool_result", () => {
    const tcs = [{ id: "tc1", name: "x", args_json: "{}" }]
    const trs = [{ tool_use_id: "tc1", content: "ok", is_error: false }]
    const joined = joinToolResults(tcs, trs)
    expect(joined).toHaveLength(1)
    expect(joined[0].id).toBe("tc1")
    expect(joined[0].result?.content).toBe("ok")
  })

  it("leaves result null when no match", () => {
    const tcs = [{ id: "orphan", name: "x", args_json: "{}" }]
    const joined = joinToolResults(tcs, [])
    expect(joined[0].result).toBeNull()
  })

  it("leaves result null when id mismatches", () => {
    const tcs = [{ id: "tc1", name: "x", args_json: "{}" }]
    const trs = [{ tool_use_id: "other", content: "ok", is_error: false }]
    const joined = joinToolResults(tcs, trs)
    expect(joined[0].result).toBeNull()
  })
})
