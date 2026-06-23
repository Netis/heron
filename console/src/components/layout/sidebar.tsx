import { NavLink, useSearchParams } from "react-router"
import {
  LayoutDashboard,
  Gauge,
  BarChart3,
  AlertTriangle,
  Server,
  Sparkles,
  MessageSquare,
  MessagesSquare,
  Network,
  PanelLeftClose,
  Settings,
  Moon,
  Sun,
  Leaf,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { useSidebarStore } from "@/stores/sidebar"
import { useThemeStore, type ThemeMode } from "@/stores/theme"
import { Logo } from "@/components/ui/logo"

/** Toolbar-level param keys that should be preserved across page navigation */
const TOOLBAR_KEYS = ["preset", "start", "end", "wire_api", "model", "server_ip", "refresh"]

const observeItems = [
  { to: "/", icon: LayoutDashboard, label: "Overview" },
  { to: "/performance", icon: Gauge, label: "Performance" },
  { to: "/traffic", icon: BarChart3, label: "Usage" },
  { to: "/errors", icon: AlertTriangle, label: "Errors" },
]

const exploreItems = [
  { to: "/services", icon: Server, label: "Services" },
  { to: "/agent-sessions", icon: MessageSquare, label: "Agent Sessions" },
  { to: "/agent-turns", icon: MessagesSquare, label: "Agent Traces" },
  { to: "/llm-calls", icon: Sparkles, label: "LLM Calls" },
  { to: "/http-exchanges", icon: Network, label: "HTTP Logs" },
]

const THEME_META: Record<ThemeMode, { icon: typeof Moon; label: string; next: ThemeMode }> = {
  dark: { icon: Moon, label: "Dark", next: "light" },
  light: { icon: Sun, label: "Light", next: "kami" },
  kami: { icon: Leaf, label: "Kami", next: "dark" },
}

function NavGroup({
  label,
  items,
  expanded,
  toolbarSearch,
}: {
  label: string
  items: typeof observeItems
  expanded: boolean
  toolbarSearch: string
}) {
  return (
    <div className="flex flex-col gap-0.5">
      {expanded && (
        <span className="mb-1 mt-3 px-3 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
          {label}
        </span>
      )}
      {!expanded && <div className="mt-2 mx-2.5 border-t border-border" />}
      {items.map((item) => (
        <NavLink
          key={item.to}
          to={`${item.to}${toolbarSearch}`}
          end={item.to === "/"}
          className={({ isActive }) =>
            cn(
              "group relative flex items-center gap-3 rounded-md px-2.5 py-2 text-sm font-medium transition-all duration-150",
              isActive
                ? "bg-sidebar-accent text-foreground before:absolute before:left-0 before:top-1/2 before:h-4 before:w-[3px] before:-translate-y-1/2 before:rounded-r-full before:bg-primary"
                : "text-muted-foreground hover:bg-sidebar-accent/50 hover:text-foreground",
              !expanded && "justify-center px-0",
            )
          }
        >
          <item.icon className="size-4 shrink-0" />
          {expanded && <span>{item.label}</span>}
          {!expanded && (
            <span className="pointer-events-none absolute left-full ml-2 hidden rounded-md bg-foreground px-2 py-1 text-xs text-background shadow-lg group-hover:block z-50">
              {item.label}
            </span>
          )}
        </NavLink>
      ))}
    </div>
  )
}

export function Sidebar() {
  const { expanded, toggle } = useSidebarStore()
  const { theme, cycleTheme } = useThemeStore()
  const [searchParams] = useSearchParams()

  // Build a search string carrying only toolbar-level params
  const toolbarSearch = (() => {
    const kept = new URLSearchParams()
    for (const key of TOOLBAR_KEYS) {
      const v = searchParams.get(key)
      if (v !== null) kept.set(key, v)
    }
    const s = kept.toString()
    return s ? `?${s}` : ""
  })()

  const themeMeta = THEME_META[theme]
  const ThemeIcon = themeMeta.icon

  return (
    <aside
      className={cn(
        "fixed left-0 top-0 z-40 flex h-full flex-col border-r border-sidebar-border bg-sidebar transition-[width] duration-200",
        expanded ? "w-[200px]" : "w-[44px]",
      )}
    >
      {/* Logo */}
      <div
        className={cn(
          "flex h-12 items-center border-b border-sidebar-border",
          expanded ? "justify-between pl-3 pr-2" : "justify-center px-2",
        )}
      >
        {expanded ? (
          <>
            <Logo variant="wordmark" className="h-5 text-foreground" />
            <button
              onClick={toggle}
              aria-label="Collapse sidebar"
              className="flex size-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-sidebar-accent hover:text-foreground"
            >
              <PanelLeftClose className="size-4" />
            </button>
          </>
        ) : (
          <button
            onClick={toggle}
            aria-label="Expand sidebar"
            title="Heron — click to expand"
            className="flex size-8 items-center justify-center rounded-md text-foreground transition-colors hover:bg-sidebar-accent"
          >
            <Logo variant="icon" className="size-5" />
          </button>
        )}
      </div>

      {/* Navigation */}
      <nav className="flex flex-1 flex-col gap-0.5 overflow-y-auto p-1.5">
        <NavGroup label="Observe" items={observeItems} expanded={expanded} toolbarSearch={toolbarSearch} />
        <NavGroup label="Explore" items={exploreItems} expanded={expanded} toolbarSearch={toolbarSearch} />
      </nav>

      {/* Bottom: Theme switcher + Settings + Version */}
      <div className="flex flex-col gap-0.5 border-t border-sidebar-border p-1.5">
        {/* Theme toggle */}
        <button
          onClick={cycleTheme}
          title={`Theme: ${themeMeta.label} → ${THEME_META[themeMeta.next].label}`}
          className={cn(
            "group relative flex items-center gap-3 rounded-md px-2.5 py-2 text-sm font-medium text-muted-foreground transition-all duration-150 hover:bg-sidebar-accent/50 hover:text-foreground",
            !expanded && "justify-center px-0",
          )}
        >
          <ThemeIcon className="size-4 shrink-0" />
          {expanded && <span>{themeMeta.label}</span>}
          {!expanded && (
            <span className="pointer-events-none absolute left-full ml-2 hidden rounded-md bg-foreground px-2 py-1 text-xs text-background shadow-lg group-hover:block z-50">
              {themeMeta.label}
            </span>
          )}
        </button>

        {/* Settings */}
        <NavLink
          to={`/settings${toolbarSearch}`}
          className={({ isActive }) =>
            cn(
              "group relative flex items-center gap-3 rounded-md px-2.5 py-2 text-sm font-medium transition-all duration-150",
              isActive
                ? "bg-sidebar-accent text-foreground before:absolute before:left-0 before:top-1/2 before:h-4 before:w-[3px] before:-translate-y-1/2 before:rounded-r-full before:bg-primary"
                : "text-muted-foreground hover:bg-sidebar-accent/50 hover:text-foreground",
              !expanded && "justify-center px-0",
            )
          }
        >
          <Settings className="size-4 shrink-0" />
          {expanded && <span>Settings</span>}
          {!expanded && (
            <span className="pointer-events-none absolute left-full ml-2 hidden rounded-md bg-foreground px-2 py-1 text-xs text-background shadow-lg group-hover:block z-50">
              Settings
            </span>
          )}
        </NavLink>

        {/* Version */}
        {expanded && (
          <span className="px-3 pb-1 pt-0.5 text-[10px] tabular-nums text-muted-foreground/50">
            v{__APP_VERSION__}
          </span>
        )}
      </div>
    </aside>
  )
}
