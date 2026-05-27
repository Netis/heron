import { create } from "zustand"
import { persist } from "zustand/middleware"

interface SidebarState {
  expanded: boolean
  toggle: () => void
  setExpanded: (expanded: boolean) => void
}

export const useSidebarStore = create<SidebarState>()(
  persist(
    (set) => ({
      expanded: false,
      toggle: () => set((s) => ({ expanded: !s.expanded })),
      setExpanded: (expanded) => set({ expanded }),
    }),
    { name: "heron-sidebar" },
  ),
)
