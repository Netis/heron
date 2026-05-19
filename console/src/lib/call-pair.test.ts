import { describe, expect, it } from "bun:test"
import { groupCalls } from "./call-pair"
import type { AgentTurnCallItem } from "@/types/api"

function call(over: Partial<AgentTurnCallItem>): AgentTurnCallItem {
  return {
    id: "c",
    sequence: 1,
    request_time: 0,
    response_time: 100,
    complete_time: 200,
    wire_api: "openai-chat",
    model: "GLM-5.1",
    status_code: 200,
    is_stream: true,
    finish_reason: "stop",
    ttft_ms: 50,
    e2e_latency_ms: 200,
    input_tokens: 100,
    output_tokens: 10,
    request_path: "/v1/chat/completions",
    client_ip: "127.0.0.1",
    client_port: 1000,
    server_ip: "127.0.0.1",
    server_port: 4000,
    request_body: null,
    response_body: null,
    request_headers: null,
    response_headers: null,
    ...over,
  }
}

describe("groupCalls", () => {
  it("leaves direct calls untouched", () => {
    const c1 = call({ id: "a", sequence: 1 })
    const c2 = call({ id: "b", sequence: 2, request_time: 500_000, client_port: 1001 })
    const g = groupCalls([c1, c2])
    expect(g.visible).toHaveLength(2)
    expect(g.hopCount).toBe(0)
    expect(g.hopsByCanonical.size).toBe(0)
  })

  it("folds a 2-leg pair (client→litellm + litellm→upstream)", () => {
    // Mirrors the user's wuneng case: seq 1 → :4000 e2e=1701ms,
    // seq 2 → :9008 e2e=1645ms, ~50ms apart on request_time.
    const c1 = call({
      id: "leg1",
      sequence: 1,
      request_time: 1779167694777,
      response_time: 1779167694800,
      complete_time: 1779167694777 + 1701,
      e2e_latency_ms: 1701,
      client_port: 60590,
      server_port: 4000,
    })
    const c2 = call({
      id: "leg2",
      sequence: 2,
      request_time: 1779167694825,
      response_time: 1779167694850,
      complete_time: 1779167694825 + 1645,
      e2e_latency_ms: 1645,
      client_port: 58950,
      server_port: 9008,
    })
    const g = groupCalls([c1, c2])
    expect(g.visible).toHaveLength(1)
    expect(g.visible[0].id).toBe("leg1") // longer span → canonical
    expect(g.hopsByCanonical.get("leg1")?.[0].id).toBe("leg2")
    expect(g.hopCount).toBe(1)
    expect(g.hopSequences.has(2)).toBe(true)
  })

  it("folds many sequential pairs in one turn", () => {
    // 6 calls = 3 logical pairs (the user's case shrunk down).
    const calls: AgentTurnCallItem[] = []
    for (let i = 0; i < 3; i++) {
      const t = i * 2000 // 2s apart pairs
      calls.push(call({
        id: `a${i}`,
        sequence: i * 2 + 1,
        request_time: t,
        complete_time: t + 1700,
        client_port: 60000 + i,
        server_port: 4000,
        input_tokens: 100 + i, // each pair distinct content
      }))
      calls.push(call({
        id: `b${i}`,
        sequence: i * 2 + 2,
        request_time: t + 50,
        complete_time: t + 1650,
        client_port: 58000 + i,
        server_port: 9008,
        input_tokens: 100 + i,
      }))
    }
    const g = groupCalls(calls)
    expect(g.visible).toHaveLength(3)
    expect(g.hopCount).toBe(3)
    // Every visible call is a canonical with exactly one hop.
    for (const v of g.visible) {
      expect(g.hopsByCanonical.get(v.id)?.length).toBe(1)
    }
  })

  it("requires distinct (client_port, server_port) — same view not paired", () => {
    // Two calls at the same net view fingerprint within 100ms — could
    // be the same client retrying; do NOT fold.
    const c1 = call({ id: "a", request_time: 0, client_port: 1000, server_port: 4000 })
    const c2 = call({ id: "b", request_time: 50, client_port: 1000, server_port: 4000 })
    const g = groupCalls([c1, c2])
    expect(g.visible).toHaveLength(2)
    expect(g.hopCount).toBe(0)
  })

  it("does not pair when tokens differ", () => {
    const c1 = call({ id: "a", request_time: 0, client_port: 1000, server_port: 4000, input_tokens: 100 })
    const c2 = call({ id: "b", request_time: 50, client_port: 2000, server_port: 9008, input_tokens: 101 })
    const g = groupCalls([c1, c2])
    expect(g.visible).toHaveLength(2)
    expect(g.hopCount).toBe(0)
  })

  it("does not pair across the 100ms time window", () => {
    const c1 = call({ id: "a", request_time: 0, client_port: 1000, server_port: 4000 })
    const c2 = call({ id: "b", request_time: 200, client_port: 2000, server_port: 9008 })
    const g = groupCalls([c1, c2])
    expect(g.visible).toHaveLength(2)
    expect(g.hopCount).toBe(0)
  })

  it("supports 3-leg cluster (haproxy br0 + docker0 + upstream)", () => {
    const a = call({
      id: "a", sequence: 1, request_time: 0, complete_time: 2000,
      client_port: 5000, server_port: 9000,
    })
    const b = call({
      id: "b", sequence: 2, request_time: 0, complete_time: 2000,
      client_ip: "172.17.0.1", server_ip: "172.17.0.9",
      client_port: 5001, server_port: 9001,
    })
    const c = call({
      id: "c", sequence: 3, request_time: 2, complete_time: 1998,
      client_ip: "172.17.0.1", server_ip: "172.17.0.4",
      client_port: 5002, server_port: 30000,
    })
    const g = groupCalls([a, b, c])
    expect(g.visible).toHaveLength(1)
    expect(g.hopCount).toBe(2)
  })

  it("pairs even when request_path differs (proxy URL rewrite)", () => {
    // Real-world LiteLLM case from wuneng: client SDK sends
    // /v1/chat/completions to LiteLLM (port 4000), LiteLLM forwards
    // bare /chat/completions to the upstream (port 9008). Same call,
    // different captured paths.
    const c1 = call({
      id: "client", sequence: 1, request_time: 1779167694777,
      complete_time: 1779167694777 + 1701, e2e_latency_ms: 1701,
      client_port: 60590, server_port: 4000,
      request_path: "/chat/completions",
    })
    const c2 = call({
      id: "upstream", sequence: 2, request_time: 1779167694825,
      complete_time: 1779167694825 + 1645, e2e_latency_ms: 1645,
      client_port: 58950, server_port: 9008,
      request_path: "/v1/chat/completions",
    })
    const g = groupCalls([c1, c2])
    expect(g.visible).toHaveLength(1)
    expect(g.hopCount).toBe(1)
  })

  it("pairs even when model differs (LiteLLM alias rewrite)", () => {
    // Live wuneng case: client SDK sends `glm5` (alias) to LiteLLM:4000,
    // LiteLLM rewrites it to `GLM-5.1` for the upstream. Same logical
    // call, different model field per leg. Model rewrite IS surfaced
    // in the Proxy view; pairing must succeed regardless.
    const c1 = call({
      id: "alias", sequence: 1, request_time: 1779171261156,
      complete_time: 1779171261156 + 3070, e2e_latency_ms: 3070,
      client_port: 48264, server_port: 4000,
      model: "glm5",
    })
    const c2 = call({
      id: "rewritten", sequence: 2, request_time: 1779171261213,
      complete_time: 1779171261213 + 2998, e2e_latency_ms: 2998,
      client_port: 40326, server_port: 9000,
      model: "GLM-5.1",
    })
    const g = groupCalls([c1, c2])
    expect(g.visible).toHaveLength(1)
    expect(g.hopCount).toBe(1)
  })

  it("folds a 4-leg topology (alias + 3 rewritten legs)", () => {
    // Exact shape from production: glm5 (LiteLLM ingress) → GLM-5.1
    // (LiteLLM egress, captured 3x at different views) — all share
    // identical tokens.
    const base = 1779171261156
    const calls: AgentTurnCallItem[] = [
      call({ id: "a", sequence: 1, request_time: base, complete_time: base + 3070, e2e_latency_ms: 3070, client_port: 48264, server_port: 4000, model: "glm5" }),
      call({ id: "b", sequence: 2, request_time: base + 57, complete_time: base + 57 + 2998, e2e_latency_ms: 2998, client_port: 40326, server_port: 9000, model: "GLM-5.1" }),
      call({ id: "c", sequence: 3, request_time: base + 58, complete_time: base + 58 + 2997, e2e_latency_ms: 2997, client_ip: "172.17.0.1", client_port: 53036, server_ip: "172.17.0.9", server_port: 9000, model: "GLM-5.1" }),
      call({ id: "d", sequence: 4, request_time: base + 60, complete_time: base + 60 + 2994, e2e_latency_ms: 2994, client_ip: "172.17.0.1", client_port: 39476, server_ip: "172.17.0.4", server_port: 30000, model: "GLM-5.1" }),
    ]
    const g = groupCalls(calls)
    expect(g.visible).toHaveLength(1)
    expect(g.hopCount).toBe(3)
  })

  it("preserves call order in the visible list", () => {
    const c1 = call({ id: "first", sequence: 1, request_time: 0, client_port: 1000, server_port: 4000 })
    const c2 = call({ id: "first-hop", sequence: 2, request_time: 50, client_port: 2000, server_port: 9008 })
    const c3 = call({ id: "second", sequence: 3, request_time: 5000, client_port: 1000, server_port: 4000, input_tokens: 200 })
    const g = groupCalls([c1, c2, c3])
    expect(g.visible.map((v) => v.id)).toEqual(["first", "second"])
  })
})
