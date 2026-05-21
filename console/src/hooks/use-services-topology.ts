import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type { ServicesTopology } from "@/types/api"

export function useServicesTopology() {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)

  return useQuery({
    queryKey: ["services-topology", { start, end }],
    queryFn: () =>
      apiFetch<ServicesTopology>("/api/services/topology", { start, end }),
    placeholderData: (prev) => prev,
  })
}
