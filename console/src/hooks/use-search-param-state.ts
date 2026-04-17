import { useCallback, useRef } from "react"
import { useSearchParams } from "react-router"

/**
 * Batches multiple setSearchParams calls within the same tick into a single
 * navigation, preventing later calls from overwriting earlier ones.
 */
const pendingUpdates = new Map<string, string | null>()
let flushScheduled = false
let flushFn: (() => void) | null = null

/**
 * useState-like hook backed by a URL search param.
 * Multiple instances can safely update different keys in the same tick.
 */
export function useSearchParamState(
  key: string,
  defaultValue: string,
): [string, (value: string) => void] {
  const [searchParams, setSearchParams] = useSearchParams()

  const value = searchParams.get(key) ?? defaultValue

  // Keep a stable ref to the latest setSearchParams so the microtask uses it
  const setRef = useRef(setSearchParams)
  setRef.current = setSearchParams

  const setValue = useCallback(
    (next: string) => {
      pendingUpdates.set(key, next === defaultValue ? null : next)

      // Register the flush function from whichever instance schedules first
      if (!flushScheduled) {
        flushScheduled = true
        flushFn = () => {
          const updates = new Map(pendingUpdates)
          pendingUpdates.clear()
          flushScheduled = false
          flushFn = null

          setRef.current(
            (prev) => {
              const p = new URLSearchParams(prev)
              for (const [k, v] of updates) {
                if (v === null) p.delete(k)
                else p.set(k, v)
              }
              return p
            },
            { replace: true },
          )
        }
        queueMicrotask(() => flushFn?.())
      }
    },
    [key, defaultValue],
  )

  return [value, setValue]
}
