import { describe, expect, it } from "bun:test"
import { buildToolIndex, classifyToolUseState, classifyToolResultState, getToolEntry, type ToolUseState } from "./turn-index"
import type { AgentTurnCallItem } from "@/types/api"

describe("buildToolIndex", () => {
  it("returns an empty map for an empty turn", () => {
    const index = buildToolIndex([])
    expect(index.size).toBe(0)
  })
})

function anthropicCall(seq: number, id: string, body: {
  reqMsgs?: Array<{ role: "user" | "assistant"; content: Array<Record<string, unknown>> }>
  respContent?: Array<Record<string, unknown>>
} = {}) {
  return {
    id,
    sequence: seq,
    request_time: 0,
    response_time: null,
    complete_time: null,
    wire_api: "anthropic",
    model: "claude-sonnet-4-6",
    status_code: 200,
    is_stream: false,
    finish_reason: null,
    ttft_ms: null,
    e2e_latency_ms: null,
    input_tokens: null,
    output_tokens: null,
    request_path: "/v1/messages",
    client_ip: "",
    client_port: 0,
    server_ip: "",
    server_port: 0,
    request_body: body.reqMsgs ? JSON.stringify({ model: "x", messages: body.reqMsgs }) : null,
    response_body: body.respContent ? JSON.stringify({ content: body.respContent, stop_reason: "tool_use", usage: {} }) : null,
    request_headers: null,
    response_headers: null,
  } satisfies Parameters<typeof buildToolIndex>[0][number]
}

describe("buildToolIndex — anthropic", () => {
  it("matches tool_use in call#1 with tool_result in call#2", () => {
    const calls = [
      anthropicCall(1, "c1", {
        reqMsgs: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
        respContent: [{ type: "tool_use", id: "tu_01", name: "Read", input: { path: "a" } }],
      }),
      anthropicCall(2, "c2", {
        reqMsgs: [
          { role: "user", content: [{ type: "text", text: "hi" }] },
          { role: "assistant", content: [{ type: "tool_use", id: "tu_01", name: "Read", input: { path: "a" } }] },
          { role: "user", content: [{ type: "tool_result", tool_use_id: "tu_01", content: "ok", is_error: false }] },
        ],
      }),
    ]
    const index = buildToolIndex(calls)
    const entry = index.get("tu_01")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.origin?.tool_name).toBe("Read")
    expect(entry?.origin?.args_json).toContain('"path": "a"')
    expect(entry?.resolution?.call_sequence).toBe(2)
    expect(entry?.resolution?.content).toBe("ok")
    expect(entry?.resolution?.is_error).toBe(false)
    expect(entry?.resolution?.size_bytes).toBe(2)
  })
})

describe("buildToolIndex — openai-chat", () => {
  it("matches tool_calls[].id with role=tool messages by tool_call_id", () => {
    const c1 = {
      id: "c1", sequence: 1, wire_api: "openai-chat", model: "gpt-4",
      request_time: 0, response_time: null, complete_time: null,
      status_code: 200, is_stream: false, finish_reason: null,
      ttft_ms: null, e2e_latency_ms: null, input_tokens: null, output_tokens: null,
      request_path: "/v1/chat/completions", client_ip: "", client_port: 0, server_ip: "", server_port: 0,
      request_body: JSON.stringify({ model: "gpt-4", messages: [{ role: "user", content: "hi" }] }),
      response_body: JSON.stringify({
        choices: [{
          index: 0, finish_reason: "tool_calls",
          message: {
            role: "assistant", content: null,
            tool_calls: [{ id: "call_01", type: "function", function: { name: "Read", arguments: "{\"p\":1}" } }],
          },
        }],
      }),
      request_headers: null, response_headers: null,
    }
    const c2 = {
      ...c1, id: "c2", sequence: 2,
      request_body: JSON.stringify({
        model: "gpt-4",
        messages: [
          { role: "user", content: "hi" },
          { role: "assistant", content: null, tool_calls: [{ id: "call_01", type: "function", function: { name: "Read", arguments: "{\"p\":1}" } }] },
          { role: "tool", tool_call_id: "call_01", content: "ok" },
        ],
      }),
      response_body: null,
    }
    const index = buildToolIndex([c1, c2] as AgentTurnCallItem[])
    const entry = index.get("call_01")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.origin?.tool_name).toBe("Read")
    expect(entry?.resolution?.call_sequence).toBe(2)
    expect(entry?.resolution?.content).toBe("ok")
  })
})

describe("buildToolIndex — openai-responses", () => {
  it("matches function_call.call_id with function_call_output.call_id", () => {
    const c1 = {
      id: "c1", sequence: 1, wire_api: "openai-responses", model: "gpt-5",
      request_time: 0, response_time: null, complete_time: null,
      status_code: 200, is_stream: false, finish_reason: null,
      ttft_ms: null, e2e_latency_ms: null, input_tokens: null, output_tokens: null,
      request_path: "/v1/responses", client_ip: "", client_port: 0, server_ip: "", server_port: 0,
      request_body: JSON.stringify({ model: "gpt-5", input: [{ type: "message", role: "user", content: [{ type: "input_text", text: "hi" }] }] }),
      response_body: JSON.stringify({
        output: [{ type: "function_call", call_id: "fc_01", name: "Read", arguments: "{\"p\":1}" }],
      }),
      request_headers: null, response_headers: null,
    }
    const c2 = {
      ...c1, id: "c2", sequence: 2,
      request_body: JSON.stringify({
        model: "gpt-5",
        input: [
          { type: "message", role: "user", content: [{ type: "input_text", text: "hi" }] },
          { type: "function_call", call_id: "fc_01", name: "Read", arguments: "{\"p\":1}" },
          { type: "function_call_output", call_id: "fc_01", output: "ok" },
        ],
      }),
      response_body: null,
    }
    const index = buildToolIndex([c1, c2] as AgentTurnCallItem[])
    const entry = index.get("fc_01")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.origin?.tool_name).toBe("Read")
    expect(entry?.resolution?.call_sequence).toBe(2)
    expect(entry?.resolution?.content).toContain("ok")
  })
})

describe("buildToolIndex — capture loss", () => {
  it("records null resolution when tool_use has no matching tool_result anywhere", () => {
    const calls = [
      anthropicCall(1, "c1", {
        respContent: [{ type: "tool_use", id: "tu_gap", name: "Read", input: {} }],
      }),
      anthropicCall(2, "c2", {
        reqMsgs: [{ role: "user", content: [{ type: "text", text: "continue" }] }],
        respContent: [{ type: "text", text: "done" }],
      }),
    ]
    const index = buildToolIndex(calls)
    const entry = index.get("tu_gap")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.resolution).toBeNull()
  })

  it("records null origin when tool_result has no matching tool_use (orphan)", () => {
    const calls = [
      anthropicCall(1, "c1", {
        reqMsgs: [
          { role: "user", content: [{ type: "tool_result", tool_use_id: "tu_orphan", content: "stray", is_error: false }] },
        ],
      }),
    ]
    const index = buildToolIndex(calls)
    const entry = index.get("tu_orphan")
    expect(entry?.origin).toBeNull()
    expect(entry?.resolution?.call_sequence).toBe(1)
  })

  it("first-wins: tool_result appearing in call#2 and #3 history records #2", () => {
    const tr = { type: "tool_result", tool_use_id: "tu_first", content: "v1", is_error: false }
    const calls = [
      anthropicCall(1, "c1", {
        respContent: [{ type: "tool_use", id: "tu_first", name: "Read", input: {} }],
      }),
      anthropicCall(2, "c2", {
        reqMsgs: [{ role: "user", content: [tr] }],
        respContent: [{ type: "text", text: "ack" }],
      }),
      anthropicCall(3, "c3", {
        // #3 carries the full history, including tr
        reqMsgs: [
          { role: "user", content: [tr] },
          { role: "assistant", content: [{ type: "text", text: "ack" }] },
          { role: "user", content: [{ type: "text", text: "continue" }] },
        ],
      }),
    ]
    const index = buildToolIndex(calls)
    expect(index.get("tu_first")?.resolution?.call_sequence).toBe(2)
  })
})

describe("buildToolIndex — client tool_id normalization (OpenClaw)", () => {
  it("links tool_use (canonical) and tool_result (stripped) via canonicalize", () => {
    // Reproduces OpenClaw's client behavior: response stream emits the
    // canonical `call_<hex>` tool_call_id, but echoes it as `call<hex>` (no
    // underscore) into subsequent messages history. ToolIndex must canonicalize
    // both sides so they collide on the same key.
    const c1 = {
      id: "c1", sequence: 1, wire_api: "openai-chat", model: "glm",
      request_time: 0, response_time: null, complete_time: null,
      status_code: 200, is_stream: false, finish_reason: null,
      ttft_ms: null, e2e_latency_ms: null, input_tokens: null, output_tokens: null,
      request_path: "/v1/chat/completions", client_ip: "", client_port: 0, server_ip: "", server_port: 0,
      request_body: JSON.stringify({ model: "glm", messages: [{ role: "user", content: "hi" }] }),
      response_body: JSON.stringify({
        choices: [{
          index: 0, finish_reason: "tool_calls",
          message: {
            role: "assistant", content: null,
            tool_calls: [{ id: "call_abcdef", type: "function", function: { name: "Exec", arguments: "{}" } }],
          },
        }],
      }),
      request_headers: null, response_headers: null,
    }
    const c2 = {
      ...c1, id: "c2", sequence: 2,
      request_body: JSON.stringify({
        model: "glm",
        messages: [
          { role: "user", content: "hi" },
          // Client stripped the underscore in echo:
          { role: "assistant", content: null, tool_calls: [{ id: "callabcdef", type: "function", function: { name: "Exec", arguments: "{}" } }] },
          { role: "tool", tool_call_id: "callabcdef", content: "ok" },
        ],
      }),
      response_body: null,
    }
    const index = buildToolIndex([c1, c2] as AgentTurnCallItem[])

    // Index is keyed canonically.
    const entry = index.get("call_abcdef")
    expect(entry?.origin?.call_sequence).toBe(1)
    expect(entry?.resolution?.call_sequence).toBe(2)
    expect(entry?.resolution?.content).toBe("ok")

    // Lookups via getToolEntry work with either form.
    expect(getToolEntry(index, "call_abcdef").origin?.call_sequence).toBe(1)
    expect(getToolEntry(index, "callabcdef").origin?.call_sequence).toBe(1)
  })
})

describe("classifyToolUseState", () => {
  it("healthy when resolution is set", () => {
    const state = classifyToolUseState(
      { origin: null, resolution: { call_sequence: 2, call_id: "c2", is_error: false, size_bytes: 0, content: "" } },
    )
    expect(state).toBe<ToolUseState>("healthy")
  })

  it("capture_gap when no resolution", () => {
    const state = classifyToolUseState({ origin: null, resolution: null })
    expect(state).toBe<ToolUseState>("capture_gap")
  })
})

describe("classifyToolResultState", () => {
  it("healthy when origin is set", () => {
    expect(classifyToolResultState({
      origin: { call_sequence: 1, call_id: "c1", tool_name: "Read", args_json: "{}" },
      resolution: null,
    })).toBe("healthy")
  })

  it("orphan when origin is null", () => {
    expect(classifyToolResultState({ origin: null, resolution: null })).toBe("orphan")
  })
})
