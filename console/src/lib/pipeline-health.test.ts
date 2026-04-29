import { describe, expect, it } from "bun:test"
import { classifyHealth, computeFunnel, FUNNEL_STAGES } from "./pipeline-health"
import type { MetricRecord } from "@/types/api"

const counter = (
  name: string,
  group: MetricRecord["group"],
  value: number,
): MetricRecord => ({ name, group, kind: "counter", value })

const cappedGauge = (
  name: string,
  group: MetricRecord["group"],
  value: number,
  capacity: number,
): MetricRecord => ({ name, group, kind: "gauge", value, capacity })

const noPrev: Record<string, number> = {}

describe("classifyHealth", () => {
  it("returns healthy when nothing's wrong", () => {
    const all = [
      counter("pkts_received", "capture", 100),
      counter("pkts_dropped_kernel", "capture", 0),
      cappedGauge("q_raw_pkts", "protocol", 100, 4096),
    ]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("healthy")
    expect(h.findings).toHaveLength(0)
  })

  it("flags critical on kernel drop delta > 0", () => {
    const all = [counter("pkts_dropped_kernel", "capture", 5)]
    const prev = { pkts_dropped_kernel: 2 }
    const h = classifyHealth(all, prev)
    expect(h.level).toBe("critical")
    expect(h.findings.some((f) => f.metric === "pkts_dropped_kernel")).toBe(true)
  })

  it("flags critical when any capped gauge >= 95%", () => {
    const all = [cappedGauge("q_raw_pkts", "protocol", 3900, 4096)]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("critical")
    expect(h.findings.some((f) => f.metric === "q_raw_pkts")).toBe(true)
  })

  it("flags warning when capped gauge >= 90% but < 95%", () => {
    const all = [cappedGauge("q_raw_pkts", "protocol", 3700, 4096)]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("warning")
  })

  it("flags warning when tcp_ooo_dropped delta > 0", () => {
    const all = [counter("tcp_ooo_dropped", "protocol", 7)]
    const prev = { tcp_ooo_dropped: 4 }
    const h = classifyHealth(all, prev)
    expect(h.level).toBe("warning")
  })

  it("flags warning on sticky cumulative > 0 even with delta = 0", () => {
    const all = [counter("tcp_ooo_dropped", "protocol", 7)]
    const prev = { tcp_ooo_dropped: 7 }
    const h = classifyHealth(all, prev)
    expect(h.level).toBe("warning")
  })

  it("critical wins over warning", () => {
    const all = [
      cappedGauge("q_raw_pkts", "protocol", 4000, 4096), // critical
      counter("tcp_ooo_dropped", "protocol", 5), // warning
    ]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("critical")
  })
})

describe("computeFunnel", () => {
  const fixture = (): MetricRecord[] => [
    counter("pkts_received", "capture", 12401),
    counter("pkts_parsed", "protocol", 12373),
    counter("pkts_dropped_not_ip", "protocol", 23),
    counter("pkts_dropped_not_tcp", "protocol", 5),
    counter("pkts_dropped_malformed", "protocol", 0),
    counter("http_exchanges_joined", "protocol", 6400),
    counter("http_exchanges_unpaired", "protocol", 2),
    counter("http_exchanges_expired", "protocol", 0),
    counter("wires_detected", "llm", 88),
    counter("wires_ignored", "llm", 6312),
    counter("calls_with_agent", "llm", 87),
    counter("calls_without_agent", "llm", 1),
    counter("calls_ingested", "turn", 87),
    counter("calls_dropped_late", "turn", 0),
    counter("calls_auxiliary", "turn", 5),
    counter("turns_completed", "turn", 22),
    counter("turns_discarded_no_user_start", "turn", 1),
    counter("flushed_calls", "storage", 87),
    counter("buf_calls", "storage", 87),
  ]

  it("emits one row per FUNNEL_STAGES entry, in order", () => {
    const rows = computeFunnel(fixture())
    expect(rows.map((r) => r.label)).toEqual(FUNNEL_STAGES.map((s) => s.label))
  })

  it("widthRatio of pkts_received is 1.0", () => {
    const rows = computeFunnel(fixture())
    const root = rows[0]
    expect(root.label).toBe("pkts_received")
    expect(root.widthRatio).toBeCloseTo(1.0)
  })

  it("widthRatio of wires_detected is 88/12401", () => {
    const rows = computeFunnel(fixture())
    const wires = rows.find((r) => r.label === "wires_detected")!
    expect(wires.widthRatio).toBeCloseTo(88 / 12401, 5)
  })

  it("annotates pkts_parsed with not_ip / not_tcp / malformed counts", () => {
    const rows = computeFunnel(fixture())
    const parsed = rows.find((r) => r.label === "pkts_parsed")!
    expect(parsed.dropAnnotation).toContain("not_ip 23")
    expect(parsed.dropAnnotation).toContain("not_tcp 5")
    expect(parsed.dropAnnotation).toContain("malformed 0")
    expect(parsed.dropAnnotation).toMatch(/-28/)
  })

  it("annotates wires_detected with wires_ignored count", () => {
    const rows = computeFunnel(fixture())
    const wires = rows.find((r) => r.label === "wires_detected")!
    expect(wires.dropAnnotation).toContain("wires_ignored 6312")
  })

  it("flags turns_completed as fan-in (not a drop)", () => {
    const rows = computeFunnel(fixture())
    const turns = rows.find((r) => r.label === "turns_completed")!
    expect(turns.dropAnnotation?.toLowerCase()).toContain("fan-in")
  })

  it("widthRatio is 0 when pkts_received is missing or zero", () => {
    const rows = computeFunnel([])
    expect(rows.every((r) => r.widthRatio === 0)).toBe(true)
  })
})
