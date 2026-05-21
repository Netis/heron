import { describe, expect, it } from "bun:test"
import { formatAxisTime } from "./format"

// Note: formatAxisTime renders in the local timezone (via Date.getHours()
// / getMonth() etc.). Tests assert the *shape* (number of segments,
// presence of date) rather than literal values, so they pass regardless
// of the runner's TZ.

const MINUTE = 60
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR
const EPOCH = 1_780_000_000 // mid-2026, arbitrary

describe("formatAxisTime", () => {
  it("renders HH:MM only when the span is under 24h", () => {
    for (const span of [15 * MINUTE, HOUR, 6 * HOUR, 23 * HOUR]) {
      const s = formatAxisTime(EPOCH, span)
      expect(s).toMatch(/^\d{2}:\d{2}$/)
    }
  })

  it("renders MM-DD HH:MM when the span is between 24h and 7d", () => {
    for (const span of [DAY, 2 * DAY, 3 * DAY, 6 * DAY]) {
      const s = formatAxisTime(EPOCH, span)
      expect(s).toMatch(/^\d{2}-\d{2} \d{2}:\d{2}$/)
    }
  })

  it("renders date-only (MM-DD) at 7d or longer", () => {
    for (const span of [7 * DAY, 14 * DAY, 30 * DAY]) {
      const s = formatAxisTime(EPOCH, span)
      expect(s).toMatch(/^\d{2}-\d{2}$/)
    }
  })

  it("falls back to HH:MM when the span is 0 (single-point data)", () => {
    expect(formatAxisTime(EPOCH, 0)).toMatch(/^\d{2}:\d{2}$/)
  })

  it("treats the 24h boundary inclusively as multi-day", () => {
    // Exactly 24h: still in the [24h, 7d) bucket → date prefix included.
    expect(formatAxisTime(EPOCH, DAY)).toMatch(/^\d{2}-\d{2} \d{2}:\d{2}$/)
  })

  it("treats the 7d boundary inclusively as date-only", () => {
    expect(formatAxisTime(EPOCH, 7 * DAY)).toMatch(/^\d{2}-\d{2}$/)
  })
})
