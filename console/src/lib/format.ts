const DAY_SECONDS = 86_400

/**
 * Chart x-axis tick label that adapts to the visible window's duration.
 *
 *   spanSec < 24h   → `HH:MM`           (5m / 15m / 1h / 6h presets)
 *   24h ≤ < 7d      → `MM-DD HH:MM`     (24h preset; gives the date on a
 *                                        few ticks so multi-day windows
 *                                        don't all read like the same
 *                                        wrapping clock face)
 *   ≥ 7d            → `MM-DD`           (7d preset; ticks come ~daily,
 *                                        the time-of-day is noise)
 *
 * Caller supplies `spanSec` (typically `end - start` of the toolbar
 * window, or `lastTs - firstTs` of the data). Centralized here so the
 * four chart components share one rule.
 */
export function formatAxisTime(epochSec: number, spanSec: number): string {
  const d = new Date(epochSec * 1000)
  const hh = String(d.getHours()).padStart(2, "0")
  const mm = String(d.getMinutes()).padStart(2, "0")
  if (spanSec < DAY_SECONDS) {
    return `${hh}:${mm}`
  }
  const mo = String(d.getMonth() + 1).padStart(2, "0")
  const da = String(d.getDate()).padStart(2, "0")
  if (spanSec < 7 * DAY_SECONDS) {
    return `${mo}-${da} ${hh}:${mm}`
  }
  return `${mo}-${da}`
}

export function formatTime(epochMs: number): string {
  const d = new Date(epochMs)
  const hh = String(d.getHours()).padStart(2, "0")
  const mm = String(d.getMinutes()).padStart(2, "0")
  const ss = String(d.getSeconds()).padStart(2, "0")
  const ms = String(d.getMilliseconds()).padStart(3, "0")
  return `${hh}:${mm}:${ss}.${ms}`
}

export function formatDateTime(epochMs: number): string {
  const d = new Date(epochMs)
  const year = d.getFullYear()
  const month = String(d.getMonth() + 1).padStart(2, "0")
  const day = String(d.getDate()).padStart(2, "0")
  const hh = String(d.getHours()).padStart(2, "0")
  const mm = String(d.getMinutes()).padStart(2, "0")
  const ss = String(d.getSeconds()).padStart(2, "0")
  return `${year}-${month}-${day} ${hh}:${mm}:${ss}`
}

export function formatDateTimeMs(epochMs: number): string {
  const d = new Date(epochMs)
  const year = d.getFullYear()
  const month = String(d.getMonth() + 1).padStart(2, "0")
  const day = String(d.getDate()).padStart(2, "0")
  const hh = String(d.getHours()).padStart(2, "0")
  const mm = String(d.getMinutes()).padStart(2, "0")
  const ss = String(d.getSeconds()).padStart(2, "0")
  const ms = String(d.getMilliseconds()).padStart(3, "0")
  return `${year}-${month}-${day} ${hh}:${mm}:${ss}.${ms}`
}

export function formatMs(ms: number | null | undefined): string {
  if (ms == null) return "—"
  if (ms < 1) return "<1ms"
  if (ms < 1000) return `${ms.toFixed(1)}ms`
  return `${(ms / 1000).toFixed(2)}s`
}

export function formatDuration(ms: number | null | undefined): string {
  if (ms == null) return "—"
  if (ms < 1000) return `${ms}ms`
  const s = ms / 1000
  if (s < 60) return `${s.toFixed(2)}s`
  const m = Math.floor(s / 60)
  const rem = s - m * 60
  if (m < 60) return `${m}m ${rem.toFixed(0)}s`
  const h = Math.floor(m / 60)
  return `${h}h ${m - h * 60}m`
}

export function formatNumber(n: number | null | undefined): string {
  if (n == null) return "—"
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return String(n)
}

/** ms-since-epoch → "16:30", "yesterday", "3d ago" */
export function formatRelativeTime(ms: number): string {
  const now = Date.now()
  const diffMs = now - ms
  const day = 86_400_000
  if (diffMs < day && new Date(ms).toDateString() === new Date(now).toDateString()) {
    const d = new Date(ms)
    return `${d.getHours().toString().padStart(2, "0")}:${d.getMinutes().toString().padStart(2, "0")}`
  }
  if (diffMs < 2 * day) return "yesterday"
  const days = Math.floor(diffMs / day)
  return `${days}d ago`
}

export function formatBytes(n: number | null | undefined): string {
  if (n == null) return "—"
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MiB`
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GiB`
}
