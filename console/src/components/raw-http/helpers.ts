/** Parse a JSON-encoded `[[name, value], ...]` into tuples. Returns [] on failure or null. */
export function parseHeaders(raw: string | null): [string, string][] {
  if (!raw) return []
  try {
    const parsed = JSON.parse(raw)
    return Array.isArray(parsed) ? parsed : []
  } catch {
    return []
  }
}

/** Pretty-print a JSON string with 2-space indent. On parse failure, return raw unchanged. */
export function formatJson(raw: string | null): string {
  if (!raw) return ""
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

/** Safe JSON.parse; returns undefined on failure. */
export function tryParseJson(raw: string | null): unknown | undefined {
  if (raw == null) return undefined
  try {
    return JSON.parse(raw)
  } catch {
    return undefined
  }
}

/** Format a byte count as "1.2 KB" / "17 B". */
export function formatSize(raw: string | null): string {
  if (!raw) return "0 B"
  const bytes = new Blob([raw]).size
  if (bytes < 1024) return `${bytes} B`
  return `${(bytes / 1024).toFixed(1)} KB`
}

/** Collapsed-object preview: up to 2 top-level keys, truncated to 60 chars. */
export function collapsedObjectPreview(obj: Record<string, unknown>): string {
  const keys = Object.keys(obj)
  if (keys.length === 0) return "{}"
  const shown = keys.slice(0, 2).map((k) => `${k}: ...`).join(", ")
  const line = `{${shown}}`
  return line.length > 60 ? `${line.slice(0, 59)}…` : line
}

/** Collapsed-array preview. */
export function collapsedArrayPreview(arr: unknown[]): string {
  return arr.length === 0 ? "[]" : `[${arr.length} items]`
}

/** Walk a parsed JSON value and yield every object/array path as a stable string key. */
export function walkAllPaths(value: unknown, path = "$"): string[] {
  const out: string[] = []
  const visit = (v: unknown, p: string) => {
    if (v === null || typeof v !== "object") return
    out.push(p)
    if (Array.isArray(v)) {
      v.forEach((item, i) => visit(item, `${p}[${i}]`))
    } else {
      for (const k of Object.keys(v as Record<string, unknown>)) {
        visit((v as Record<string, unknown>)[k], `${p}.${k}`)
      }
    }
  }
  visit(value, path)
  return out
}

/** Build the default expansion map: first two nesting levels are open. */
export function defaultExpansion(value: unknown): Record<string, boolean> {
  const map: Record<string, boolean> = {}
  const visit = (v: unknown, p: string, depth: number) => {
    if (v === null || typeof v !== "object") return
    if (depth < 2) map[p] = true
    if (Array.isArray(v)) {
      v.forEach((item, i) => visit(item, `${p}[${i}]`, depth + 1))
    } else {
      for (const k of Object.keys(v as Record<string, unknown>)) {
        visit((v as Record<string, unknown>)[k], `${p}.${k}`, depth + 1)
      }
    }
  }
  visit(value, "$", 0)
  return map
}
