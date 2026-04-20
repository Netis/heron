import { create } from "zustand"

export type TimeRangePreset = "5m" | "15m" | "1h" | "6h" | "24h" | "7d" | "custom"

export const PRESET_SECONDS: Record<Exclude<TimeRangePreset, "custom">, number> = {
  "5m": 5 * 60,
  "15m": 15 * 60,
  "1h": 60 * 60,
  "6h": 6 * 60 * 60,
  "24h": 24 * 60 * 60,
  "7d": 7 * 24 * 60 * 60,
}

const VALID_PRESETS = new Set<string>(["5m", "15m", "1h", "6h", "24h", "7d", "custom"])

export function isValidPreset(v: string): v is TimeRangePreset {
  return VALID_PRESETS.has(v)
}

export interface DimensionFilters {
  wireApi: string
  model: string
  serverIp: string
}

/** Default values — used to decide whether a param should appear in the URL */
export const TOOLBAR_DEFAULTS = {
  preset: "1h" as TimeRangePreset,
  wireApi: "",
  model: "",
  serverIp: "",
  refreshInterval: 0,
} as const

interface ToolbarState {
  preset: TimeRangePreset
  /** Epoch seconds — always kept in sync */
  start: number
  end: number
  /** Dimension filters — CSV strings, empty = all */
  filters: DimensionFilters
  /** Auto-refresh interval in ms (0 = off) */
  refreshInterval: number
  setPreset: (preset: Exclude<TimeRangePreset, "custom">) => void
  setCustomRange: (start: number, end: number) => void
  setFilter: (key: keyof DimensionFilters, value: string) => void
  setRefreshInterval: (interval: number) => void
  /** Batch-set state from URL params (used by sync hook) */
  _hydrate: (patch: {
    preset?: TimeRangePreset
    start?: number
    end?: number
    filters?: Partial<DimensionFilters>
    refreshInterval?: number
  }) => void
}

function nowSeconds() {
  return Math.floor(Date.now() / 1000)
}

export const useToolbarStore = create<ToolbarState>((set) => {
  const now = nowSeconds()
  return {
    preset: "1h",
    start: now - PRESET_SECONDS["1h"],
    end: now,
    filters: { wireApi: "", model: "", serverIp: "" },
    refreshInterval: 0,
    setPreset: (preset) => {
      const now = nowSeconds()
      set({
        preset,
        start: now - PRESET_SECONDS[preset],
        end: now,
      })
    },
    setCustomRange: (start, end) => {
      set({ preset: "custom", start, end, refreshInterval: 0 })
    },
    setFilter: (key, value) => {
      set((state) => ({ filters: { ...state.filters, [key]: value } }))
    },
    setRefreshInterval: (interval) => {
      set({ refreshInterval: interval })
    },
    _hydrate: (patch) => {
      set((state) => ({
        ...(patch.preset !== undefined && { preset: patch.preset }),
        ...(patch.start !== undefined && { start: patch.start }),
        ...(patch.end !== undefined && { end: patch.end }),
        ...(patch.refreshInterval !== undefined && { refreshInterval: patch.refreshInterval }),
        ...(patch.filters && {
          filters: { ...state.filters, ...patch.filters },
        }),
      }))
    },
  }
})
