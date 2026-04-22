import { describe, expect, it } from "bun:test"
import { readFileSync } from "node:fs"
import path from "node:path"
import { parseOpenAiResponsesCall } from "./index"

function fx(name: string): string {
  return readFileSync(path.resolve(__dirname, "__fixtures__", name), "utf8")
}

describe("parseOpenAiResponsesCall — request", () => {
  it("instructions extracted verbatim", () => {
    const call = parseOpenAiResponsesCall(fx("openai_responses_input_full.json"), null)
    expect(call.request.instructions).toBe("You are a code assistant.")
  })

  it("full fixture: items are message + function_call + function_call_output", () => {
    const call = parseOpenAiResponsesCall(fx("openai_responses_input_full.json"), null)
    const kinds = call.request.input.map((i) => i.kind)
    expect(kinds).toEqual(["message", "function_call", "function_call_output"])
  })

  it("function_call item preserves call_id, name, arguments (JSON string)", () => {
    const call = parseOpenAiResponsesCall(fx("openai_responses_input_full.json"), null)
    const fc = call.request.input[1]
    if (fc.kind === "function_call") {
      expect(fc.call_id).toBe("call_abc")
      expect(fc.name).toBe("run_shell")
      expect(fc.arguments).toBe('{"cmd":"ls"}')
    } else {
      throw new Error("expected function_call")
    }
  })

  it("function_call_output preserves raw output value (string case)", () => {
    const call = parseOpenAiResponsesCall(fx("openai_responses_input_full.json"), null)
    const fco = call.request.input[2]
    if (fco.kind === "function_call_output") {
      expect(fco.call_id).toBe("call_abc")
      expect(fco.output).toBe("a.txt\nb.txt")
    } else {
      throw new Error("expected function_call_output")
    }
  })

  it("function_call_output with object output keeps the object (not stringified)", () => {
    const body = JSON.stringify({
      model: "gpt-5",
      input: [
        {
          type: "function_call_output",
          call_id: "call_x",
          output: { status: "ok", data: [1, 2, 3] },
        },
      ],
    })
    const call = parseOpenAiResponsesCall(body, null)
    const fco = call.request.input[0]
    if (fco.kind === "function_call_output") {
      expect(fco.output).toEqual({ status: "ok", data: [1, 2, 3] })
    } else {
      throw new Error("expected function_call_output")
    }
  })

  it("plain string input becomes single user message", () => {
    const call = parseOpenAiResponsesCall(JSON.stringify({ model: "gpt-5", input: "hi" }), null)
    expect(call.request.input).toHaveLength(1)
    expect(call.request.input[0].kind).toBe("message")
    if (call.request.input[0].kind === "message") {
      expect(call.request.input[0].role).toBe("user")
      expect(call.request.input[0].content).toBe("hi")
    }
  })

  it("typeless item with role + content is treated as message", () => {
    const call = parseOpenAiResponsesCall(
      JSON.stringify({
        model: "gpt-4o",
        input: [
          {
            role: "user",
            content: [
              { type: "input_text", text: "what's in this image?" },
              { type: "input_image", image_url: "https://x/img.png" },
            ],
          },
        ],
      }),
      null,
    )
    expect(call.request.input).toHaveLength(1)
    expect(call.request.input[0].kind).toBe("message")
    if (call.request.input[0].kind === "message" && Array.isArray(call.request.input[0].content)) {
      expect(call.request.input[0].content[0].type).toBe("input_text")
      expect(call.request.input[0].content[1].type).toBe("input_image")
    }
  })

  it("reasoning item preserves summary strings", () => {
    const body = JSON.stringify({
      model: "gpt-5",
      input: [
        {
          type: "reasoning",
          id: "rs_1",
          summary: [{ type: "summary_text", text: "first step" }, { type: "summary_text", text: "second step" }],
        },
      ],
    })
    const call = parseOpenAiResponsesCall(body, null)
    const r = call.request.input[0]
    if (r.kind === "reasoning") {
      expect(r.id).toBe("rs_1")
      expect(r.summary).toEqual(["first step", "second step"])
    } else {
      throw new Error("expected reasoning")
    }
  })

  it("tools preserve native per-type fields (function + file_search)", () => {
    const body = JSON.stringify({
      model: "gpt-5",
      tools: [
        { type: "function", name: "run_shell", description: "run a cmd", parameters: { type: "object" }, strict: true },
        { type: "file_search", vector_store_ids: ["vs_1", "vs_2"] },
      ],
    })
    const call = parseOpenAiResponsesCall(body, null)
    expect(call.request.tools).toHaveLength(2)
    expect(call.request.tools[0].type).toBe("function")
    expect(call.request.tools[0].strict).toBe(true)
    expect(call.request.tools[1].type).toBe("file_search")
    expect(call.request.tools[1].vector_store_ids).toEqual(["vs_1", "vs_2"])
  })

  it("sampling extracts previous_response_id / store / metadata / truncation / include", () => {
    const body = JSON.stringify({
      model: "gpt-5",
      max_output_tokens: 1024,
      previous_response_id: "resp_prev",
      store: true,
      metadata: { user: "u1" },
      truncation: "auto",
      include: ["reasoning.encrypted_content"],
    })
    const call = parseOpenAiResponsesCall(body, null)
    expect(call.request.sampling.max_output_tokens).toBe(1024)
    expect(call.request.sampling.previous_response_id).toBe("resp_prev")
    expect(call.request.sampling.store).toBe(true)
    expect(call.request.sampling.metadata).toEqual({ user: "u1" })
    expect(call.request.sampling.truncation).toBe("auto")
    expect(call.request.sampling.include).toEqual(["reasoning.encrypted_content"])
  })
})

describe("parseOpenAiResponsesCall — response", () => {
  it("message output aggregates output_text into output_text_aggregated", () => {
    const call = parseOpenAiResponsesCall(null, fx("openai_responses_output_message.json"))
    expect(call.response.output_text_aggregated.length).toBeGreaterThan(0)
  })

  it("function_call output with reasoning has both", () => {
    const call = parseOpenAiResponsesCall(null, fx("openai_responses_output_function_call.json"))
    const kinds = call.response.output.map((o) => o.kind)
    expect(kinds).toContain("function_call")
    expect(kinds).toContain("reasoning")
  })

  it("usage input_tokens_details.cached_tokens mapped to cached_input_tokens", () => {
    const body = JSON.stringify({
      id: "resp_1",
      status: "completed",
      output: [],
      usage: {
        input_tokens: 100,
        input_tokens_details: { cached_tokens: 80 },
        output_tokens: 50,
        output_tokens_details: { reasoning_tokens: 30 },
        total_tokens: 150,
      },
    })
    const call = parseOpenAiResponsesCall(null, body)
    expect(call.response.usage.cached_input_tokens).toBe(80)
    expect(call.response.usage.reasoning_tokens).toBe(30)
  })

  it("status preserved; other non-standard statuses pass through", () => {
    const call = parseOpenAiResponsesCall(null, JSON.stringify({ status: "in_progress" }))
    expect(call.response.status).toBe("in_progress")
  })
})
