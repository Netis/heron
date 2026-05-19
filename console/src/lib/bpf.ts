/**
 * Round-trip BPF helpers for the Settings page.
 *
 * The page only exposes a structured editor for the two common cases —
 * filter by port(s) and/or filter by host(s) — and synthesises a BPF
 * string from them. Anything else (icmp, vlan, not, complex expressions
 * with mixed and/or, …) falls back to raw mode. The parser is the gate:
 * if it can recognise the existing filter as the synth's output, structured
 * mode opens cleanly; otherwise the editor stays in raw.
 *
 * Intentionally narrow: we don't try to cover every BPF expression. The
 * structured view is a convenience for the common case, not a general BPF
 * GUI.
 */

export interface StructuredBpf {
  /** TCP/UDP ports (positive integers, ≤ 65535). */
  ports: number[]
  /** Host IPv4 / IPv6 literals or hostnames. We don't validate beyond non-empty. */
  hosts: string[]
}

const IPV4 = /^(\d{1,3})(?:\.\d{1,3}){3}(?:\/\d{1,2})?$/
const HOST_TOKEN = /^[A-Za-z0-9._:/-]+$/ // permissive — covers IPv4, simple IPv6, hostnames

/**
 * Synthesise a BPF filter string from a structured filter. Returns the
 * empty string when both arrays are empty.
 *
 * Shape produced (matches what the parser accepts):
 *
 *     <ports>            ports only: `tcp port 80 or tcp port 443`
 *     <hosts>            hosts only: `host 1.2.3.4 or host 5.6.7.8`
 *     <ports> and <hosts> both: `(tcp port 80 or tcp port 443) and (host 1.2.3.4)`
 */
export function synthBpf(s: StructuredBpf): string {
  const portsPart = s.ports.length > 0 ? s.ports.map((p) => `tcp port ${p}`).join(" or ") : ""
  const hostsPart = s.hosts.length > 0 ? s.hosts.map((h) => `host ${h}`).join(" or ") : ""

  if (portsPart && hostsPart) {
    const lp = s.ports.length > 1 ? `(${portsPart})` : portsPart
    const lh = s.hosts.length > 1 ? `(${hostsPart})` : hostsPart
    return `${lp} and ${lh}`
  }
  return portsPart || hostsPart
}

/**
 * Try to interpret a BPF string as a structured filter. Returns null when
 * the string contains anything outside the structured grammar — the caller
 * should then drop into raw mode.
 *
 * Accepted shapes (case-insensitive `and`/`or`/`port`/`host`/`tcp`):
 *   - empty / whitespace-only
 *   - `tcp port N` (one or more, joined by `or`, optional outer parens)
 *   - `host H`     (one or more, joined by `or`, optional outer parens)
 *   - <ports> AND <hosts> in either order
 */
export function parseBpf(raw: string | null | undefined): StructuredBpf | null {
  if (raw == null) return { ports: [], hosts: [] }
  const trimmed = raw.trim()
  if (trimmed === "") return { ports: [], hosts: [] }

  // Split on top-level " and " — but not inside parens.
  const parts = splitTopLevel(trimmed, "and")
  if (parts.length === 0 || parts.length > 2) return null

  const ports: number[] = []
  const hosts: string[] = []
  let sawPorts = false
  let sawHosts = false

  for (const partRaw of parts) {
    const part = stripOuterParens(partRaw.trim())
    const tokens = splitTopLevel(part, "or").map((t) => t.trim())
    if (tokens.length === 0) return null

    // Each token in this clause must be the same kind (port|host). Mixed
    // tokens inside one and-clause is fine on the wire but doesn't fit
    // the structured editor — fall back to raw.
    let kind: "port" | "host" | null = null
    for (const t of tokens) {
      const p = matchPortToken(t)
      const h = matchHostToken(t)
      if (p !== null) {
        if (kind === "host") return null
        kind = "port"
        ports.push(p)
      } else if (h !== null) {
        if (kind === "port") return null
        kind = "host"
        hosts.push(h)
      } else {
        return null
      }
    }
    if (kind === "port") {
      if (sawPorts) return null // two port-clauses joined by `and` is weird; raw
      sawPorts = true
    } else if (kind === "host") {
      if (sawHosts) return null
      sawHosts = true
    }
  }

  return { ports: dedupNum(ports), hosts: dedupStr(hosts) }
}

// ---- token matchers --------------------------------------------------------

function matchPortToken(t: string): number | null {
  // Allow "tcp port 80" and "port 80". The synth always emits "tcp port N",
  // but raw configs in the wild also use bare "port N".
  const m = t.match(/^(?:tcp\s+)?port\s+(\d{1,5})$/i)
  if (!m) return null
  const n = Number(m[1])
  if (!Number.isInteger(n) || n < 1 || n > 65535) return null
  return n
}

function matchHostToken(t: string): string | null {
  const m = t.match(/^host\s+(\S+)$/i)
  if (!m) return null
  const v = m[1]
  // Sanity check on the value — IPv4/CIDR or a plausible hostname/IPv6.
  if (IPV4.test(v) || HOST_TOKEN.test(v)) return v
  return null
}

// ---- string helpers --------------------------------------------------------

function stripOuterParens(s: string): string {
  if (s.length < 2 || s[0] !== "(" || s[s.length - 1] !== ")") return s
  // Ensure the outer pair is balanced — `(a) or (b)` shouldn't be stripped.
  let depth = 0
  for (let i = 0; i < s.length; i++) {
    if (s[i] === "(") depth++
    else if (s[i] === ")") {
      depth--
      if (depth === 0 && i !== s.length - 1) return s
    }
  }
  return s.slice(1, -1).trim()
}

/** Split `s` on whole-word `sep` (case-insensitive) at paren-depth 0. */
function splitTopLevel(s: string, sep: "and" | "or"): string[] {
  const out: string[] = []
  const lower = s.toLowerCase()
  const sepRe = new RegExp(`\\s${sep}\\s`, "i")
  let depth = 0
  let start = 0
  let i = 0
  while (i < s.length) {
    const c = s[i]
    if (c === "(") depth++
    else if (c === ")") depth--
    else if (depth === 0 && i + sep.length + 2 <= s.length) {
      // Look for "<sep>" surrounded by whitespace at this position.
      const window = lower.slice(i, i + sep.length + 2)
      if (sepRe.test(window) && /\s/.test(window[0]) && /\s/.test(window[window.length - 1])) {
        out.push(s.slice(start, i))
        i += sep.length + 2
        start = i
        continue
      }
    }
    i++
  }
  out.push(s.slice(start))
  return out.map((p) => p.trim()).filter(Boolean)
}

function dedupNum(xs: number[]): number[] {
  return Array.from(new Set(xs)).sort((a, b) => a - b)
}
function dedupStr(xs: string[]): string[] {
  return Array.from(new Set(xs))
}

// ---- validation ------------------------------------------------------------

export function isValidPort(s: string): boolean {
  const n = Number(s)
  return Number.isInteger(n) && n >= 1 && n <= 65535
}

export function isValidHost(s: string): boolean {
  return s.length > 0 && (IPV4.test(s) || HOST_TOKEN.test(s))
}
