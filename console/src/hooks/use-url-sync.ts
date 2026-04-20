import { useEffect, useRef } from "react"
import { useSearchParams } from "react-router"
import {
  useToolbarStore,
  PRESET_SECONDS,
  TOOLBAR_DEFAULTS,
  isValidPreset,
} from "@/stores/toolbar"

function nowSeconds() {
  return Math.floor(Date.now() / 1000)
}

/** URL param keys for toolbar state */
const P = {
  preset: "preset",
  start: "start",
  end: "end",
  wireApi: "wire_api",
  model: "model",
  serverIp: "server_ip",
  refresh: "refresh",
} as const

/**
 * Bidirectional sync between the toolbar Zustand store and URL search params.
 * Mount once in AppLayout.
 */
export function useToolbarUrlSync() {
  const [searchParams, setSearchParams] = useSearchParams()
  const skipUrlUpdate = useRef(false)

  // ── URL → Store (on mount & on searchParams change from popstate) ──
  useEffect(() => {
    const urlPreset = searchParams.get(P.preset)

    // Parse URL params into a store hydration patch
    const hydratePatch: Parameters<ReturnType<typeof useToolbarStore.getState>["_hydrate"]>[0] = {}

    if (urlPreset && isValidPreset(urlPreset)) {
      hydratePatch.preset = urlPreset
      if (urlPreset === "custom") {
        const s = searchParams.get(P.start)
        const e = searchParams.get(P.end)
        if (s && e) {
          hydratePatch.start = Number(s)
          hydratePatch.end = Number(e)
        }
      } else {
        const now = nowSeconds()
        hydratePatch.start = now - PRESET_SECONDS[urlPreset]
        hydratePatch.end = now
      }
    }

    const wireApi = searchParams.get(P.wireApi)
    const model = searchParams.get(P.model)
    const serverIp = searchParams.get(P.serverIp)
    if (wireApi !== null || model !== null || serverIp !== null) {
      hydratePatch.filters = {
        ...(wireApi !== null && { wireApi }),
        ...(model !== null && { model }),
        ...(serverIp !== null && { serverIp }),
      }
    }

    const refresh = searchParams.get(P.refresh)
    if (refresh !== null) {
      hydratePatch.refreshInterval = Number(refresh)
    }

    if (Object.keys(hydratePatch).length > 0) {
      skipUrlUpdate.current = true
      useToolbarStore.getState()._hydrate(hydratePatch)
      // If hydration was a no-op (store already matched URL), Zustand won't
      // fire subscribers, so the flag never resets. Clear it in a microtask
      // to ensure the next real store change isn't swallowed.
      queueMicrotask(() => { skipUrlUpdate.current = false })
    }
  }, [searchParams])

  // ── Store → URL (on store change) ──
  useEffect(() => {
    const unsub = useToolbarStore.subscribe((state) => {
      if (skipUrlUpdate.current) {
        skipUrlUpdate.current = false
        return
      }
      const params = storeToParams(state)
      setSearchParams(
        (prev) => {
          const p = new URLSearchParams(prev)
          // Clear old toolbar keys first
          for (const k of Object.values(P)) p.delete(k)
          for (const [k, v] of params) p.set(k, v)
          return p
        },
        { replace: true },
      )
    })
    return unsub
  }, [setSearchParams])
}

/** Serialize store state to URL param entries (omitting defaults) */
function storeToParams(
  state: ReturnType<typeof useToolbarStore.getState>,
): [string, string][] {
  const entries: [string, string][] = []

  if (state.preset !== TOOLBAR_DEFAULTS.preset) {
    entries.push([P.preset, state.preset])
  }
  if (state.preset === "custom") {
    entries.push([P.start, String(state.start)])
    entries.push([P.end, String(state.end)])
  }
  if (state.filters.wireApi !== TOOLBAR_DEFAULTS.wireApi) {
    entries.push([P.wireApi, state.filters.wireApi])
  }
  if (state.filters.model !== TOOLBAR_DEFAULTS.model) {
    entries.push([P.model, state.filters.model])
  }
  if (state.filters.serverIp !== TOOLBAR_DEFAULTS.serverIp) {
    entries.push([P.serverIp, state.filters.serverIp])
  }
  if (state.refreshInterval !== TOOLBAR_DEFAULTS.refreshInterval) {
    entries.push([P.refresh, String(state.refreshInterval)])
  }

  return entries
}
