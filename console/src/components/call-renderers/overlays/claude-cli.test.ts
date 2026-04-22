import { describe, expect, it } from "bun:test"
import { segmentClaudeCliUserText } from "./claude-cli-segment"

describe("segmentClaudeCliUserText", () => {
  it("pure text becomes a single plain segment", () => {
    const segs = segmentClaudeCliUserText("hello world")
    expect(segs).toHaveLength(1)
    expect(segs[0]).toEqual({ kind: "plain", text: "hello world" })
  })

  it("isolates <system-reminder> blocks", () => {
    const input = "prefix <system-reminder>secret note</system-reminder> suffix"
    const segs = segmentClaudeCliUserText(input)
    expect(segs.map((s) => s.kind)).toEqual(["plain", "system-reminder", "plain"])
    if (segs[1].kind === "system-reminder") {
      expect(segs[1].body).toBe("secret note")
    }
  })

  it("handles multiple consecutive <system-reminder> blocks", () => {
    const input = "<system-reminder>a</system-reminder>middle<system-reminder>b</system-reminder>end"
    const segs = segmentClaudeCliUserText(input)
    expect(segs.map((s) => s.kind)).toEqual(["system-reminder", "plain", "system-reminder", "plain"])
  })

  it("parses a full command triple (name + message + args)", () => {
    const input = "<command-name>plan</command-name><command-message>plan</command-message><command-args>--fix</command-args>"
    const segs = segmentClaudeCliUserText(input)
    expect(segs).toHaveLength(1)
    expect(segs[0].kind).toBe("command")
    if (segs[0].kind === "command") {
      expect(segs[0].name).toBe("plan")
      expect(segs[0].message).toBe("plan")
      expect(segs[0].args).toBe("--fix")
    }
  })

  it("command with only name (no message / args) still parses", () => {
    const input = "<command-name>fast</command-name>and back"
    const segs = segmentClaudeCliUserText(input)
    expect(segs).toHaveLength(2)
    expect(segs[0].kind).toBe("command")
    if (segs[0].kind === "command") {
      expect(segs[0].name).toBe("fast")
      expect(segs[0].message).toBe("")
      expect(segs[0].args).toBe("")
    }
    expect(segs[1]).toEqual({ kind: "plain", text: "and back" })
  })

  it("isolates <local-command-stdout>", () => {
    const input = "<local-command-stdout>line1\nline2</local-command-stdout>"
    const segs = segmentClaudeCliUserText(input)
    expect(segs).toHaveLength(1)
    expect(segs[0].kind).toBe("local-command-stdout")
    if (segs[0].kind === "local-command-stdout") {
      expect(segs[0].body).toBe("line1\nline2")
    }
  })

  it("unclosed <system-reminder> spills remainder as plain (safe-fail)", () => {
    const input = "text <system-reminder>oops no close"
    const segs = segmentClaudeCliUserText(input)
    // "text " plain, then the rest (including the opening tag) as plain.
    expect(segs[0]).toEqual({ kind: "plain", text: "text " })
    expect(segs[1].kind).toBe("plain")
  })

  it("mixes plain + command + system-reminder in a typical user message", () => {
    const input = "<command-name>plan</command-name><command-message>m</command-message><command-args>a</command-args>User: Hi <system-reminder>internal</system-reminder>"
    const segs = segmentClaudeCliUserText(input)
    expect(segs.map((s) => s.kind)).toEqual(["command", "plain", "system-reminder"])
  })
})
