import { useCallback } from "react"
import { useSearchParams } from "react-router"

export function useTurnUrlState() {
  const [params, setParams] = useSearchParams()
  const call = params.get("call") ? Number(params.get("call")) : null
  const raw = params.get("raw") === "1"

  const setCall = useCallback((seq: number | null) => {
    setParams((prev) => {
      const next = new URLSearchParams(prev)
      if (seq == null) {
        next.delete("call")
        next.delete("raw")
      } else {
        next.set("call", String(seq))
      }
      return next
    }, { replace: true })
  }, [setParams])

  const setRaw = useCallback((on: boolean) => {
    setParams((prev) => {
      const next = new URLSearchParams(prev)
      if (on) next.set("raw", "1")
      else next.delete("raw")
      return next
    }, { replace: true })
  }, [setParams])

  const openRaw = useCallback((seq: number) => {
    setParams((prev) => {
      const next = new URLSearchParams(prev)
      next.set("call", String(seq))
      next.set("raw", "1")
      return next
    }, { replace: true })
  }, [setParams])

  return { call, raw, setCall, setRaw, openRaw }
}
