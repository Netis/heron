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

const gauge = (
  name: string,
  group: MetricRecord["group"],
  value: number,
): MetricRecord => ({ name, group, kind: "gauge", value })

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
    counter("pkts_routed", "protocol", 12401),
    counter("pkts_parsed", "protocol", 12373),
    counter("pkts_dropped_not_ip", "protocol", 23),
    counter("pkts_dropped_not_tcp", "protocol", 5),
    counter("pkts_dropped_malformed", "protocol", 0),
    counter("http_reqs_parsed", "protocol", 6402),
    counter("http_resps_parsed", "protocol", 6400),
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
    counter("windows_emitted", "metrics", 30),
    counter("flushed_calls", "storage", 87),
    counter("flushed_turns", "storage", 22),
    counter("flushed_exchanges", "storage", 6400),
    counter("flushed_metrics", "storage", 30),
    // buf_* are gauges of the WriteBuffer's current pending batch length
    // (0 ≤ value ≤ batch_size). Steady-state values are typically small.
    gauge("buf_calls", "storage", 3),
    gauge("buf_turns", "storage", 1),
    gauge("buf_exchanges", "storage", 150),
    gauge("buf_metrics", "storage", 2),
  ]

  it("emits one row per FUNNEL_STAGES entry, in order", () => {
    const rows = computeFunnel(fixture())
    expect(rows.map((r) => r.label)).toEqual(FUNNEL_STAGES.map((s) => s.label))
  })

  it("root row (pkts_received) has ratio 1.0 and kind=root", () => {
    const rows = computeFunnel(fixture())
    const root = rows[0]
    expect(root.label).toBe("pkts_received")
    expect(root.kind).toBe("root")
    expect(root.widthRatio).toBeCloseTo(1.0)
    expect(root.upstream).toBeNull()
  })

  it("normal rows ratio against their direct upstream", () => {
    const rows = computeFunnel(fixture())
    // http_exchanges_joined (6400) vs http_resps_parsed (6400) = 1.0
    const ex = rows.find((r) => r.label === "http_exchanges_joined")!
    expect(ex.kind).toBe("normal")
    expect(ex.upstream).toBe("http_resps_parsed")
    expect(ex.widthRatio).toBeCloseTo(1.0, 5)
  })

  it("filter rows are tagged 'filter' and surface upstream", () => {
    const rows = computeFunnel(fixture())
    const wires = rows.find((r) => r.label === "wires_detected")!
    expect(wires.kind).toBe("filter")
    expect(wires.upstream).toBe("http_exchanges_joined")
    expect(wires.widthRatio).toBeCloseTo(88 / 6400, 5)
  })

  it("info rows have null ratio and null upstream", () => {
    const rows = computeFunnel(fixture())
    for (const lbl of [
      "http_reqs_parsed",
      "windows_emitted",
      "flushed_metrics",
    ] as const) {
      const r = rows.find((r) => r.label === lbl)!
      expect(r.kind).toBe("info")
      expect(r.widthRatio).toBeNull()
      expect(r.upstream).toBeNull()
    }
  })

  it("fanIn row (turns_completed) has null ratio", () => {
    const rows = computeFunnel(fixture())
    const turns = rows.find((r) => r.label === "turns_completed")!
    expect(turns.kind).toBe("fanIn")
    expect(turns.widthRatio).toBeNull()
  })

  it("normal storage rows ratio against their producer", () => {
    const rows = computeFunnel(fixture())
    const fcalls = rows.find((r) => r.label === "flushed_calls")!
    const fturns = rows.find((r) => r.label === "flushed_turns")!
    const fexch = rows.find((r) => r.label === "flushed_exchanges")!
    expect(fcalls.upstream).toBe("calls_with_agent")
    expect(fcalls.widthRatio).toBeCloseTo(87 / 87, 5)
    expect(fturns.upstream).toBe("turns_completed")
    expect(fturns.widthRatio).toBeCloseTo(22 / 22, 5)
    expect(fexch.upstream).toBe("http_exchanges_joined")
    expect(fexch.widthRatio).toBeCloseTo(6400 / 6400, 5)
  })

  it("annotates pkts_parsed with not_ip / not_tcp / malformed counts", () => {
    const rows = computeFunnel(fixture())
    const parsed = rows.find((r) => r.label === "pkts_parsed")!
    expect(parsed.dropAnnotation).toContain("not_ip 23")
    expect(parsed.dropAnnotation).toContain("not_tcp 5")
    expect(parsed.dropAnnotation).toContain("malformed 0")
    expect(parsed.dropAnnotation).toMatch(/-28/)
  })

  it("annotates wires_detected with wires_ignored count and 'filter' label", () => {
    const rows = computeFunnel(fixture())
    const wires = rows.find((r) => r.label === "wires_detected")!
    expect(wires.dropAnnotation).toContain("wires_ignored 6312")
    expect(wires.dropAnnotation?.toLowerCase()).toContain("filter")
  })

  it("flags turns_completed as fan-in (not a drop)", () => {
    const rows = computeFunnel(fixture())
    const turns = rows.find((r) => r.label === "turns_completed")!
    expect(turns.dropAnnotation?.toLowerCase()).toContain("fan-in")
  })

  it("storage rows are tagged with their producer in annotation", () => {
    const rows = computeFunnel(fixture())
    expect(
      rows.find((r) => r.label === "flushed_calls")!.dropAnnotation,
    ).toContain("from llm")
    expect(
      rows.find((r) => r.label === "flushed_turns")!.dropAnnotation,
    ).toContain("from turn")
    expect(
      rows.find((r) => r.label === "flushed_exchanges")!.dropAnnotation,
    ).toContain("from joiner")
    expect(
      rows.find((r) => r.label === "flushed_metrics")!.dropAnnotation,
    ).toContain("from metrics")
  })

  it("ratios are 0 (or null for info/fanIn) when no metrics are reported", () => {
    const rows = computeFunnel([])
    for (const r of rows) {
      if (r.kind === "info" || r.kind === "fanIn") {
        expect(r.widthRatio).toBeNull()
      } else {
        expect(r.widthRatio).toBe(0)
      }
    }
  })
})
