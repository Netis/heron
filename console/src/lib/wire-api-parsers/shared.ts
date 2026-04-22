export function asString(v: unknown): string | null {
  return typeof v === "string" ? v : null
}

export function asArray(v: unknown): unknown[] | null {
  return Array.isArray(v) ? v : null
}

export function asObject(v: unknown): Record<string, unknown> | null {
  return v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null
}

export function asNumber(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null
}

export function asBoolean(v: unknown): boolean | null {
  return typeof v === "boolean" ? v : null
}

export function asUint(v: unknown): number | null {
  const n = asNumber(v)
  return n != null && n >= 0 && Number.isInteger(n) ? n : null
}

export function get(obj: unknown, key: string): unknown {
  const o = asObject(obj)
  return o ? o[key] : undefined
}

/** Mirror serde_json::to_string — stable JSON stringify with no whitespace. */
export function toJsonString(v: unknown): string {
  try {
    return JSON.stringify(v) ?? ""
  } catch {
    return ""
  }
}

/** Parse a JSON string; return null on any failure. */
export function parseJsonOrNull(s: string | null | undefined): unknown {
  if (s == null) return null
  try {
    return JSON.parse(s)
  } catch {
    return null
  }
}

export function stringOrJson(v: unknown): string {
  if (typeof v === "string") return v
  if (v == null) return ""
  return toJsonString(v)
}
