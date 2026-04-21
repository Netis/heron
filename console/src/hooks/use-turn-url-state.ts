import { useCallback } from "react"
import { useSearchParams } from "react-router"

export function useTurnUrlState() {
  const [params, setParams] = useSearchParams()
  const call = params.get("call") ? Number(params.get("call")) : null
  const raw = params.get("raw") === "1"

  const setCall = useCallback((seq: number | null) => {
    const next = new URLSearchParams(params)
    if (seq == null) next.delete("call")
    else next.set("call", String(seq))
    if (seq == null) next.delete("raw")
    setParams(next, { replace: true })
  }, [params, setParams])

  const setRaw = useCallback((on: boolean) => {
    const next = new URLSearchParams(params)
    if (on) next.set("raw", "1")
    else next.delete("raw")
    setParams(next, { replace: true })
  }, [params, setParams])

  return { call, raw, setCall, setRaw }
}
