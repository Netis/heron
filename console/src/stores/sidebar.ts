import { create } from "zustand"

interface SidebarState {
  expanded: boolean
  toggle: () => void
  setExpanded: (expanded: boolean) => void
}

export const useSidebarStore = create<SidebarState>((set) => ({
  expanded: false,
  toggle: () => set((s) => ({ expanded: !s.expanded })),
  setExpanded: (expanded) => set({ expanded }),
}))
