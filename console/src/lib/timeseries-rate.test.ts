import { describe, expect, it } from "bun:test"
import { GRANULARITY_SECONDS, pickGranularity, toPerSecond } from "./timeseries-rate"
import type { TimeseriesData } from "@/types/api"

const data: TimeseriesData = {
  timestamps: [0, 60],
  series: [
    { name: "total_output_tokens", group: null, values: [6000, null] },
    { name: "total_input_tokens", group: null, values: [0, 120] },
  ],
}

// Far enough past every bucket above that none of them is still open.
const END = 10_000

describe("toPerSecond", () => {
  it("drops a bucket that has not ended yet instead of charting a dip", () => {
    // 1m buckets at 0 and 60; the window ends 30s into the second one.
    const r = toPerSecond(data, "1m", 90)
    expect(r.series[1].values).toEqual([0, null])
    // ...and keeps it once the bucket is complete
    expect(toPerSecond(data, "1m", 120).series[1].values).toEqual([0, 2])
  })

  it("divides each bucket total by the bucket span", () => {
    const r = toPerSecond(data, "1m", END)
    expect(r.series[0].values).toEqual([100, null])
    expect(r.series[1].values).toEqual([0, 2])
    expect(r.timestamps).toEqual([0, 60])
  })

  it("keeps a missing bucket missing rather than a zero rate", () => {
    expect(toPerSecond(data, "10s", END).series[0].values[1]).toBeNull()
  })

  it("gives the same rate for the same traffic at any granularity", () => {
    // 100 tok/s for five minutes: one 5m bucket, or five 1m buckets.
    const coarse = toPerSecond(
      { timestamps: [0], series: [{ name: "x", group: null, values: [30_000] }] },
      "5m",
      END,
    )
    const fine = toPerSecond(
      { timestamps: [0], series: [{ name: "x", group: null, values: [6_000] }] },
      "1m",
      END,
    )
    expect(coarse.series[0].values[0]).toBe(100)
    expect(fine.series[0].values[0]).toBe(100)
  })

  it("rejects a granularity it does not know the span of", () => {
    expect(() => toPerSecond(data, "15m", END)).toThrow()
  })
})

describe("pickGranularity", () => {
  it("only picks granularities toPerSecond can convert", () => {
    for (const range of [60, 900, 901, 7200, 7201, 86400, 86401, 30 * 86400]) {
      expect(GRANULARITY_SECONDS[pickGranularity(range)]).toBeGreaterThan(0)
    }
  })
})
