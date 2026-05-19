import { describe, expect, test } from "bun:test"
import { parseBpf, synthBpf } from "./bpf"

describe("synthBpf", () => {
  test("empty → empty string", () => {
    expect(synthBpf({ ports: [], hosts: [] })).toBe("")
  })
  test("single port", () => {
    expect(synthBpf({ ports: [80], hosts: [] })).toBe("tcp port 80")
  })
  test("multiple ports", () => {
    expect(synthBpf({ ports: [80, 443], hosts: [] })).toBe(
      "tcp port 80 or tcp port 443",
    )
  })
  test("single host", () => {
    expect(synthBpf({ ports: [], hosts: ["10.0.0.1"] })).toBe("host 10.0.0.1")
  })
  test("ports and hosts", () => {
    expect(
      synthBpf({ ports: [4210, 4271], hosts: ["10.0.0.1"] }),
    ).toBe("(tcp port 4210 or tcp port 4271) and host 10.0.0.1")
  })
})

describe("parseBpf", () => {
  test("null → empty structured", () => {
    expect(parseBpf(null)).toEqual({ ports: [], hosts: [] })
  })
  test("empty string → empty structured", () => {
    expect(parseBpf("")).toEqual({ ports: [], hosts: [] })
    expect(parseBpf("   ")).toEqual({ ports: [], hosts: [] })
  })
  test("single port", () => {
    expect(parseBpf("tcp port 80")).toEqual({ ports: [80], hosts: [] })
  })
  test("bare 'port' (without tcp) still parses", () => {
    expect(parseBpf("port 80")).toEqual({ ports: [80], hosts: [] })
  })
  test("multiple ports", () => {
    expect(parseBpf("tcp port 4210 or tcp port 4271")).toEqual({
      ports: [4210, 4271],
      hosts: [],
    })
  })
  test("hosts and ports combined", () => {
    expect(
      parseBpf("(tcp port 4210 or tcp port 4271) and host 10.0.0.1"),
    ).toEqual({ ports: [4210, 4271], hosts: ["10.0.0.1"] })
  })
  test("non-structured filter → null", () => {
    expect(parseBpf("udp")).toBeNull()
    expect(parseBpf("tcp and not host 1.2.3.4")).toBeNull()
    expect(parseBpf("vlan 10")).toBeNull()
  })
  test("port out of range → null", () => {
    expect(parseBpf("tcp port 99999")).toBeNull()
  })
  test("round-trip", () => {
    const fixtures = [
      { ports: [80], hosts: [] },
      { ports: [80, 443], hosts: [] },
      { ports: [], hosts: ["10.0.0.1"] },
      { ports: [4210, 4271], hosts: ["10.0.0.1"] },
    ]
    for (const f of fixtures) {
      const round = parseBpf(synthBpf(f))
      expect(round).toEqual(f)
    }
  })
})
