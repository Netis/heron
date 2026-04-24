import { useMemo } from "react"
import { useLocation } from "react-router"
import { useToolbarStore } from "@/stores/toolbar"
import { getSpecForPath, type DimensionKey } from "@/stores/page-filter-specs"

export interface SupportedFilterParams {
  wire_api?: string
  model?: string
  server_ip?: string
}

/**
 * Intersect toolbar-store filters with the current route's supported dimensions.
 * Returns API-ready params (omitted keys when unsupported or empty) and the
 * resolved spec for rendering decisions.
 */
export function useSupportedFilterParams(): {
  spec: readonly DimensionKey[]
  params: SupportedFilterParams
} {
  const { pathname } = useLocation()
  const filters = useToolbarStore((s) => s.filters)

  const spec = useMemo(() => getSpecForPath(pathname), [pathname])

  const params = useMemo<SupportedFilterParams>(() => {
    const out: SupportedFilterParams = {}
    if (spec.includes("wireApi") && filters.wireApi) out.wire_api = filters.wireApi
    if (spec.includes("model") && filters.model) out.model = filters.model
    if (spec.includes("serverIp") && filters.serverIp) out.server_ip = filters.serverIp
    return out
  }, [spec, filters.wireApi, filters.model, filters.serverIp])

  return { spec, params }
}
