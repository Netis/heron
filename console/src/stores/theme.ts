import { create } from "zustand"
import { persist } from "zustand/middleware"

export type ThemeMode = "dark" | "light" | "kami"

interface ThemeState {
  theme: ThemeMode
  setTheme: (theme: ThemeMode) => void
  cycleTheme: () => void
}

const THEME_ORDER: ThemeMode[] = ["dark", "light", "kami"]

export const useThemeStore = create<ThemeState>()(
  persist(
    (set, get) => ({
      theme: "dark" as ThemeMode,
      setTheme: (theme) => set({ theme }),
      cycleTheme: () => {
        const current = get().theme
        const idx = THEME_ORDER.indexOf(current)
        const next = THEME_ORDER[(idx + 1) % THEME_ORDER.length]
        set({ theme: next })
      },
    }),
    { name: "heron-theme" },
  ),
)

/** Call once at app startup to sync the theme class onto <html>. */
export function initTheme() {
  const apply = (theme: ThemeMode) => {
    const html = document.documentElement
    html.classList.remove("dark", "light", "kami")
    html.classList.add(theme)
  }
  // Apply current value immediately
  apply(useThemeStore.getState().theme)
  // Re-apply on every change
  useThemeStore.subscribe((s) => apply(s.theme))
}
