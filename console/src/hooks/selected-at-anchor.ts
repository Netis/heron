/**
 * Window-anchor override for fresh URL loads with a stale relative preset.
 *
 * When a list page links to an item with `?selected=<id>&selected_at=<unix_s>`
 * and a recipient opens that URL hours later, the relative preset (e.g.
 * `preset=15m`) bracketed to "now" would no longer contain the item. This
 * helper detects that gap and shifts the window so the item lands at the
 * trailing edge:
 *
 *   - Keep the preset's *duration* — that's the "show me this much
 *     context" signal the original user picked.
 *   - Slide the window so `end = selectedAt + ANCHOR_PAD_SECONDS`,
 *     putting the item just past the right edge with a small breathing
 *     pad (so it shows up reliably in a desc-by-time list).
 *   - Promote `preset` to `"custom"` so the URL serializer writes absolute
 *     `start`/`end` back out and the shift survives subsequent navigation.
 *
 * No-ops when the anchor is absent, unparseable, or already inside the
 * computed window.
 *
 * Stand-alone module (no `@/` aliases) so it's directly testable under bun
 * without dragging in the toolbar store / react-router runtime deps.
 */

export const ANCHOR_PAD_SECONDS = 60

/** Duration to fall back to when the caller's patch has no preset-derived
 *  window. Matches the toolbar's default `1h` preset — duplicated here
 *  rather than imported so this module stays dependency-free. */
export const DEFAULT_FALLBACK_DURATION_SECONDS = 3600

export interface AnchorablePatch {
  preset?: string
  start?: number
  end?: number
}

export function applySelectedAtAnchor(
  patch: AnchorablePatch,
  selectedAtRaw: string | null,
  nowSec: number,
): void {
  if (selectedAtRaw == null) return
  const selectedAt = Number(selectedAtRaw)
  if (!Number.isFinite(selectedAt)) return

  // Treat an empty patch as the "default preset" window — 1h ending at now.
  // Matches what the toolbar store falls back to when no preset param is in
  // the URL.
  const start = patch.start ?? nowSec - DEFAULT_FALLBACK_DURATION_SECONDS
  const end = patch.end ?? nowSec
  if (selectedAt >= start && selectedAt <= end) return

  const duration = end - start
  const newEnd = selectedAt + ANCHOR_PAD_SECONDS
  const newStart = newEnd - duration
  patch.preset = "custom"
  patch.start = newStart
  patch.end = newEnd
}
