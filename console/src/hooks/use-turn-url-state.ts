import { useCallback } from "react"
import { useSearchParams } from "react-router"

export function useTurnUrlState() {
  const [params, setParams] = useSearchParams()
  const call = params.get("call") ? Number(params.get("call")) : null
  const detail = params.get("detail") === "1"

  const setCall = useCallback((seq: number | null) => {
    setParams((prev) => {
      const next = new URLSearchParams(prev)
      if (seq == null) {
        next.delete("call")
        next.delete("detail")
      } else {
        next.set("call", String(seq))
      }
      return next
    }, { replace: true })
  }, [setParams])

  const setDetail = useCallback((on: boolean) => {
    setParams((prev) => {
      const next = new URLSearchParams(prev)
      if (on) next.set("detail", "1")
      else next.delete("detail")
      return next
    }, { replace: true })
  }, [setParams])

  const openDetail = useCallback((seq: number) => {
    setParams((prev) => {
      const next = new URLSearchParams(prev)
      next.set("call", String(seq))
      next.set("detail", "1")
      return next
    }, { replace: true })
  }, [setParams])

  return { call, detail, setCall, setDetail, openDetail }
}
