import { useEffect, useRef } from "react"
import { useLocation, useSearchParams } from "react-router"
import {
  useToolbarStore,
  PRESET_SECONDS,
  TOOLBAR_DEFAULTS,
  isValidPreset,
} from "@/stores/toolbar"
import { getSpecForPath, type DimensionKey } from "@/stores/page-filter-specs"
import { applySelectedAtAnchor } from "./selected-at-anchor"

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
 * Page-level "selected item" anchor — written by list pages alongside
 * `?selected=<id>` so the recipient of a shared link can recover the
 * window the item was in, not just the most-recent N minutes.
 */
const SELECTED_AT_PARAM = "selected_at"

/**
 * Bidirectional sync between the toolbar Zustand store and URL search params.
 * Mount once in AppLayout.
 *
 * URL → store: always hydrates any filter key present in the URL (off-screen
 * values in the store are preserved across route changes).
 *
 * Store → URL: per-route. Only params supported by the active route's spec
 * are written. On route change, the URL is re-serialized so unsupported params
 * are silently stripped.
 */
export function useToolbarUrlSync() {
  const [searchParams, setSearchParams] = useSearchParams()
  const { pathname } = useLocation()
  const skipUrlUpdate = useRef(false)
  // The selected_at anchor exists to rescue a STALE shared link — when
  // the recipient opens a URL whose relative preset has moved past the
  // selected item, shift the window so the item is in view. Applying
  // it on every searchParams change wrecks the live page: every click
  // updates ?selected_at=, the URL→store effect re-runs, and the helper
  // sees the (slightly newer) window boundary as having drifted past
  // the just-clicked item — shifting unexpectedly. Gate the anchor to
  // the very first hydration in this tab.
  const anchorAppliedOnce = useRef(false)

  // ── URL → Store (on mount & on searchParams change from popstate) ──
  useEffect(() => {
    const urlPreset = searchParams.get(P.preset)

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

    if (!anchorAppliedOnce.current) {
      const selectedAtRaw = searchParams.get(SELECTED_AT_PARAM)
      applySelectedAtAnchor(hydratePatch, selectedAtRaw, nowSeconds())
      anchorAppliedOnce.current = true
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

  // ── Store → URL (per-route; re-runs on pathname change) ──
  // On every route change: (1) re-serialize store to URL using the new spec
  // so params unsupported by the current page are stripped; (2) re-subscribe
  // so future store mutations serialize against the new spec.
  useEffect(() => {
    const spec = getSpecForPath(pathname)

    writeStoreToUrl(useToolbarStore.getState(), spec, setSearchParams)

    const unsub = useToolbarStore.subscribe((state) => {
      if (skipUrlUpdate.current) {
        skipUrlUpdate.current = false
        return
      }
      writeStoreToUrl(state, spec, setSearchParams)
    })
    return unsub
  }, [pathname, setSearchParams])
}

function writeStoreToUrl(
  state: ReturnType<typeof useToolbarStore.getState>,
  spec: readonly DimensionKey[],
  setSearchParams: ReturnType<typeof useSearchParams>[1],
) {
  const params = storeToParams(state, spec)
  setSearchParams(
    (prev) => {
      const p = new URLSearchParams(prev)
      // Clear all toolbar keys first, then write supported entries
      for (const k of Object.values(P)) p.delete(k)
      for (const [k, v] of params) p.set(k, v)
      return p
    },
    { replace: true },
  )
}

/** Serialize store state to URL param entries (omitting defaults and
 *  dimension filters not in the active route's spec). */
function storeToParams(
  state: ReturnType<typeof useToolbarStore.getState>,
  spec: readonly DimensionKey[],
): [string, string][] {
  const entries: [string, string][] = []

  if (state.preset !== TOOLBAR_DEFAULTS.preset) {
    entries.push([P.preset, state.preset])
  }
  if (state.preset === "custom") {
    entries.push([P.start, String(state.start)])
    entries.push([P.end, String(state.end)])
  }
  if (spec.includes("wireApi") && state.filters.wireApi !== TOOLBAR_DEFAULTS.wireApi) {
    entries.push([P.wireApi, state.filters.wireApi])
  }
  if (spec.includes("model") && state.filters.model !== TOOLBAR_DEFAULTS.model) {
    entries.push([P.model, state.filters.model])
  }
  if (spec.includes("serverIp") && state.filters.serverIp !== TOOLBAR_DEFAULTS.serverIp) {
    entries.push([P.serverIp, state.filters.serverIp])
  }
  if (state.refreshInterval !== TOOLBAR_DEFAULTS.refreshInterval) {
    entries.push([P.refresh, String(state.refreshInterval)])
  }

  return entries
}
