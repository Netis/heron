import { describe, expect, it } from "bun:test"
import { applySelectedAtAnchor } from "./selected-at-anchor"

// Fixed "now" so test expectations don't drift with wall-clock time.
const NOW = 1_780_000_000 // unix seconds (sometime in mid-2026)
const HOUR = 3600
const MIN = 60

describe("applySelectedAtAnchor", () => {
  it("no-ops when the anchor param is absent", () => {
    const patch: { preset?: string; start?: number; end?: number } = {
      preset: "15m",
      start: NOW - 15 * MIN,
      end: NOW,
    }
    applySelectedAtAnchor(patch, null, NOW)
    expect(patch.preset).toBe("15m")
    expect(patch.start).toBe(NOW - 15 * MIN)
    expect(patch.end).toBe(NOW)
  })

  it("no-ops when the anchor is unparseable", () => {
    const patch: { preset?: string; start?: number; end?: number } = {
      preset: "15m",
      start: NOW - 15 * MIN,
      end: NOW,
    }
    applySelectedAtAnchor(patch, "not-a-number", NOW)
    expect(patch.preset).toBe("15m")
    expect(patch.start).toBe(NOW - 15 * MIN)
    expect(patch.end).toBe(NOW)
  })

  it("no-ops when the anchor is inside the window", () => {
    // Anchor 5 minutes before "now"; falls inside the 15-minute window.
    const patch: { preset?: string; start?: number; end?: number } = {
      preset: "15m",
      start: NOW - 15 * MIN,
      end: NOW,
    }
    applySelectedAtAnchor(patch, String(NOW - 5 * MIN), NOW)
    expect(patch.preset).toBe("15m")
    expect(patch.start).toBe(NOW - 15 * MIN)
    expect(patch.end).toBe(NOW)
  })

  it("shifts a stale 15m window to bracket an anchor 30 minutes ago", () => {
    // The motivating scenario: someone copied a `?preset=15m&selected_at=...`
    // URL half an hour ago; the recipient opens it now.
    const patch: { preset?: string; start?: number; end?: number } = {
      preset: "15m",
      start: NOW - 15 * MIN,
      end: NOW,
    }
    const anchor = NOW - 30 * MIN
    applySelectedAtAnchor(patch, String(anchor), NOW)
    // Promoted to custom (so the URL serializes absolute start/end).
    expect(patch.preset).toBe("custom")
    // Duration preserved (15 minutes), end is anchor + 60s pad,
    // anchor sits at the trailing edge of the window (with the pad).
    expect(patch.end).toBe(anchor + 60)
    expect(patch.start).toBe(anchor + 60 - 15 * MIN)
    expect(patch.end! - patch.start!).toBe(15 * MIN)
  })

  it("treats a missing preset window as the default 1h preset", () => {
    // No `preset` in URL → effective window is `[now-1h, now]` per the
    // toolbar default. An anchor 2h before now is outside that.
    const patch: { preset?: string; start?: number; end?: number } = {}
    const anchor = NOW - 2 * HOUR
    applySelectedAtAnchor(patch, String(anchor), NOW)
    expect(patch.preset).toBe("custom")
    expect(patch.end).toBe(anchor + 60)
    expect(patch.start).toBe(anchor + 60 - HOUR)
  })

  it("preserves the user's chosen 1h duration when overriding", () => {
    // User shared with preset=1h; recipient opens 4 hours later.
    const patch: { preset?: string; start?: number; end?: number } = {
      preset: "1h",
      start: NOW - HOUR,
      end: NOW,
    }
    const anchor = NOW - 5 * HOUR
    applySelectedAtAnchor(patch, String(anchor), NOW)
    expect(patch.preset).toBe("custom")
    expect(patch.end! - patch.start!).toBe(HOUR)
    // Anchor lands inside the new window with a 60s breathing pad.
    expect(anchor).toBeGreaterThanOrEqual(patch.start!)
    expect(anchor).toBeLessThanOrEqual(patch.end!)
  })

  it("handles anchors after the window end (clock skew / future-dated)", () => {
    // Defensive case — an anchor in the future of the recipient's clock.
    // The override should still bracket it (no assumption that anchor < now).
    const patch: { preset?: string; start?: number; end?: number } = {
      preset: "15m",
      start: NOW - 15 * MIN,
      end: NOW,
    }
    const anchor = NOW + 10 * MIN
    applySelectedAtAnchor(patch, String(anchor), NOW)
    expect(patch.preset).toBe("custom")
    expect(patch.end).toBe(anchor + 60)
    expect(patch.start).toBe(anchor + 60 - 15 * MIN)
  })
})
