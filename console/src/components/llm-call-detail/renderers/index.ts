import { CallOutputRenderer, DefaultCallRenderer } from "./default"
import type { CallRenderer } from "./types"

const registry: Record<string, CallRenderer> = {
  // Extension point: per-wire_api or per-agent_kind renderers can be registered
  // here without touching the default path. Until a provider has a reason to
  // diverge, everything falls through to DefaultCallRenderer.
}

export function getCallRenderer(wireApi: string): CallRenderer {
  return registry[wireApi] ?? DefaultCallRenderer
}

export { CallOutputRenderer, DefaultCallRenderer }
export type { CallRenderer, CallRendererProps } from "./types"
