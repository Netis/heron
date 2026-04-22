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
  const month = String(d.getMonth() + 1).padStart(2, "0")
  const day = String(d.getDate()).padStart(2, "0")
  const hh = String(d.getHours()).padStart(2, "0")
  const mm = String(d.getMinutes()).padStart(2, "0")
  const ss = String(d.getSeconds()).padStart(2, "0")
  return `${month}-${day} ${hh}:${mm}:${ss}`
}

export function formatDateTimeMs(epochMs: number): string {
  const d = new Date(epochMs)
  const month = String(d.getMonth() + 1).padStart(2, "0")
  const day = String(d.getDate()).padStart(2, "0")
  const hh = String(d.getHours()).padStart(2, "0")
  const mm = String(d.getMinutes()).padStart(2, "0")
  const ss = String(d.getSeconds()).padStart(2, "0")
  const ms = String(d.getMilliseconds()).padStart(3, "0")
  return `${month}-${day} ${hh}:${mm}:${ss}.${ms}`
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

export function formatBytes(n: number | null | undefined): string {
  if (n == null) return "—"
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MiB`
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GiB`
}
