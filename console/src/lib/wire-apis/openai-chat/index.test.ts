import { describe, expect, it } from "bun:test"
import { readFileSync } from "node:fs"
import path from "node:path"
import { parseOpenAiChatCall } from "./index"

function fx(name: string): string {
  return readFileSync(path.resolve(__dirname, "__fixtures__", name), "utf8")
}

describe("parseOpenAiChatCall — request", () => {
  it("full fixture: roles include system, user, assistant, tool, assistant", () => {
    const call = parseOpenAiChatCall(fx("openai_chat_input_full.json"), null)
    const roles = call.request.messages.map((m) => m.role)
    expect(roles).toEqual(["system", "user", "assistant", "tool", "assistant"])
  })

  it("full fixture: assistant-with-tool_calls carries tool_calls array (no flattening)", () => {
    const call = parseOpenAiChatCall(fx("openai_chat_input_full.json"), null)
    const assistant = call.request.messages[2]
    expect(assistant.tool_calls).toBeDefined()
    expect(assistant.tool_calls).toHaveLength(1)
    expect(assistant.tool_calls?.[0].function.name).toBe("get_weather")
    // Native: arguments stays as a JSON string verbatim
    expect(assistant.tool_calls?.[0].function.arguments).toBe('{"city":"SF"}')
  })

  it("full fixture: tool message preserves tool_call_id + string content", () => {
    const call = parseOpenAiChatCall(fx("openai_chat_input_full.json"), null)
    const tool = call.request.messages[3]
    expect(tool.role).toBe("tool")
    expect(tool.tool_call_id).toBe("call_1")
    expect(tool.content).toBe("72F sunny")
  })

  it("full fixture: tools[] preserves raw parameters (no input_schema_json stringification)", () => {
    const call = parseOpenAiChatCall(fx("openai_chat_input_full.json"), null)
    expect(call.request.tools).toHaveLength(1)
    expect(call.request.tools[0].function.name).toBe("get_weather")
    expect(call.request.tools[0].function.parameters).toEqual({
      type: "object",
      properties: { city: { type: "string" } },
    })
  })

  it("full fixture: response_format parsed as json_object kind", () => {
    const call = parseOpenAiChatCall(fx("openai_chat_input_full.json"), null)
    expect(call.request.response_format).toEqual({ kind: "json_object" })
  })

  it("full fixture: sampling preserves OpenAI-native fields", () => {
    const call = parseOpenAiChatCall(fx("openai_chat_input_full.json"), null)
    expect(call.request.sampling.temperature).toBe(0.3)
    expect(call.request.sampling.max_tokens).toBe(2048)
    expect(call.request.sampling.top_p).toBe(1.0)
    expect(call.request.sampling.stream).toBe(true)
    expect(call.request.sampling.stop).toEqual(["\n\n"])
    expect(call.request.sampling.tool_choice).toBe("auto")
  })

  it("response_format json_schema is parsed into structured shape", () => {
    const body = JSON.stringify({
      model: "gpt-4o",
      messages: [{ role: "user", content: "hi" }],
      response_format: {
        type: "json_schema",
        json_schema: {
          name: "answer",
          schema: { type: "object", properties: { a: { type: "string" } } },
          strict: true,
        },
      },
    })
    const call = parseOpenAiChatCall(body, null)
    expect(call.request.response_format?.kind).toBe("json_schema")
    if (call.request.response_format?.kind === "json_schema") {
      expect(call.request.response_format.name).toBe("answer")
      expect(call.request.response_format.strict).toBe(true)
    }
  })

  it("multi-modal user content parts preserved", () => {
    const body = JSON.stringify({
      model: "gpt-4o",
      messages: [
        {
          role: "user",
          content: [
            { type: "text", text: "describe" },
            { type: "image_url", image_url: { url: "https://x/img.png", detail: "high" } },
          ],
        },
      ],
    })
    const call = parseOpenAiChatCall(body, null)
    const parts = call.request.messages[0].content
    expect(Array.isArray(parts)).toBe(true)
    if (Array.isArray(parts)) {
      expect(parts[0].type).toBe("text")
      expect(parts[1].type).toBe("image_url")
      if (parts[1].type === "image_url") {
        expect(parts[1].image_url.detail).toBe("high")
      }
    }
  })
})

describe("parseOpenAiChatCall — response", () => {
  it("text output: choices[0].message.content is a string", () => {
    const call = parseOpenAiChatCall(null, fx("openai_chat_output_text.json"))
    expect(call.response.choices).toHaveLength(1)
    expect(typeof call.response.choices[0].message.content).toBe("string")
  })

  it("tool_calls output: choices[0].message.tool_calls populated", () => {
    const call = parseOpenAiChatCall(null, fx("openai_chat_output_tool_calls.json"))
    const msg = call.response.choices[0].message
    expect(msg.tool_calls).toBeDefined()
    expect(msg.tool_calls?.[0].id.startsWith("call_")).toBe(true)
  })

  it("usage includes cached_prompt_tokens and reasoning_tokens when present", () => {
    const body = JSON.stringify({
      choices: [{ index: 0, message: { role: "assistant", content: "hi" }, finish_reason: "stop" }],
      usage: {
        prompt_tokens: 100,
        completion_tokens: 20,
        total_tokens: 120,
        prompt_tokens_details: { cached_tokens: 80 },
        completion_tokens_details: { reasoning_tokens: 15 },
      },
    })
    const call = parseOpenAiChatCall(null, body)
    expect(call.response.usage.prompt_tokens).toBe(100)
    expect(call.response.usage.cached_prompt_tokens).toBe(80)
    expect(call.response.usage.reasoning_tokens).toBe(15)
  })

  it("logprobs parsed into structured entries", () => {
    const body = JSON.stringify({
      choices: [
        {
          index: 0,
          message: { role: "assistant", content: "hi" },
          finish_reason: "stop",
          logprobs: {
            content: [
              {
                token: "hi",
                logprob: -0.5,
                bytes: [104, 105],
                top_logprobs: [
                  { token: "hi", logprob: -0.5, bytes: [104, 105] },
                  { token: "hello", logprob: -1.2, bytes: null },
                ],
              },
            ],
          },
        },
      ],
    })
    const call = parseOpenAiChatCall(null, body)
    expect(call.response.choices[0].logprobs).not.toBeNull()
    expect(call.response.choices[0].logprobs?.[0].token).toBe("hi")
    expect(call.response.choices[0].logprobs?.[0].top_logprobs).toHaveLength(2)
  })

  it("system_fingerprint + service_tier preserved", () => {
    const body = JSON.stringify({
      id: "chatcmpl-1",
      model: "gpt-4o",
      system_fingerprint: "fp_abc",
      service_tier: "default",
      choices: [{ index: 0, message: { role: "assistant", content: "hi" }, finish_reason: "stop" }],
    })
    const call = parseOpenAiChatCall(null, body)
    expect(call.response.system_fingerprint).toBe("fp_abc")
    expect(call.response.service_tier).toBe("default")
  })
})
