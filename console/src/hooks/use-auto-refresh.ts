import { useEffect } from "react"
import { useToolbarStore } from "@/stores/toolbar"

export function useAutoRefresh() {
  const refreshInterval = useToolbarStore((s) => s.refreshInterval)
  const preset = useToolbarStore((s) => s.preset)
  const setPreset = useToolbarStore((s) => s.setPreset)

  useEffect(() => {
    if (refreshInterval <= 0 || preset === "custom") return

    const id = setInterval(() => {
      const current = useToolbarStore.getState().preset
      if (current !== "custom") {
        setPreset(current as Exclude<typeof current, "custom">)
      }
    }, refreshInterval)

    return () => clearInterval(id)
  }, [refreshInterval, preset, setPreset])
}
