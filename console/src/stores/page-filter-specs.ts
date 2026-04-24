import { matchPath } from "react-router"
import type { DimensionFilters } from "./toolbar"

export type DimensionKey = keyof DimensionFilters

export const ALL_DIMENSIONS: readonly DimensionKey[] = ["wireApi", "model", "serverIp"]

/**
 * Per-route dimension filter support. Ordered most-specific first —
 * `getSpecForPath` walks top-to-bottom and returns the first match.
 * Unknown paths fall through to `[]` (conservative: no filters).
 */
const SPEC_ENTRIES: ReadonlyArray<readonly [string, readonly DimensionKey[]]> = [
  ["/agent-sessions/:source_id/:session_id", []],
  ["/agent-sessions", []],
  ["/http-exchanges", ["serverIp"]],
  ["/agent-turns", ["wireApi", "model"]],
  ["/llm-calls", ["wireApi", "model", "serverIp"]],
  ["/models", ["wireApi", "model", "serverIp"]],
  ["/errors", ["wireApi", "model", "serverIp"]],
  ["/traffic", ["wireApi", "model", "serverIp"]],
  ["/performance", ["wireApi", "model", "serverIp"]],
  ["/", ["wireApi", "model", "serverIp"]],
]

const EMPTY_SPEC: readonly DimensionKey[] = []

function normalize(pathname: string): string {
  const trimmed = pathname.replace(/\/+$/, "")
  return trimmed === "" ? "/" : trimmed
}

export function getSpecForPath(pathname: string): readonly DimensionKey[] {
  const p = normalize(pathname)
  for (const [pattern, spec] of SPEC_ENTRIES) {
    if (matchPath({ path: pattern, end: true }, p)) {
      return spec
    }
  }
  return EMPTY_SPEC
}
