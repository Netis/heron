import { NavLink, useSearchParams } from "react-router"
import {
  LayoutDashboard,
  Gauge,
  BarChart3,
  AlertTriangle,
  Cpu,
  Server,
  Sparkles,
  MessageSquare,
  MessagesSquare,
  Network,
  PanelLeftClose,
  Settings,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { useSidebarStore } from "@/stores/sidebar"
import { Logo } from "@/components/ui/logo"

/** Toolbar-level param keys that should be preserved across page navigation */
const TOOLBAR_KEYS = ["preset", "start", "end", "wire_api", "model", "server_ip", "refresh"]

const navItems = [
  { to: "/", icon: LayoutDashboard, label: "Overview" },
  { to: "/performance", icon: Gauge, label: "Performance" },
  { to: "/traffic", icon: BarChart3, label: "Traffic" },
  { to: "/errors", icon: AlertTriangle, label: "Errors" },
  { to: "/models", icon: Cpu, label: "Models" },
  { to: "/services", icon: Server, label: "Services" },
  { to: "/agent-sessions", icon: MessageSquare, label: "Agent Sessions" },
  { to: "/agent-turns", icon: MessagesSquare, label: "Agent Turns" },
  { to: "/llm-calls", icon: Sparkles, label: "LLM Calls" },
  { to: "/http-exchanges", icon: Network, label: "HTTP Exchanges" },
  { to: "/settings", icon: Settings, label: "Settings" },
]

export function Sidebar() {
  const { expanded, toggle } = useSidebarStore()
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

  return (
    <aside
      className={cn(
        "fixed left-0 top-0 z-40 flex h-full flex-col border-r border-border bg-background transition-[width] duration-200",
        expanded ? "w-[200px]" : "w-[44px]",
      )}
    >
      <div
        className={cn(
          "flex h-12 items-center border-b border-border",
          expanded ? "justify-between pl-3 pr-2" : "justify-center px-2",
        )}
      >
        {expanded ? (
          <>
            <Logo variant="wordmark" className="h-5 text-foreground" />
            <button
              onClick={toggle}
              aria-label="Collapse sidebar"
              className="flex size-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <PanelLeftClose className="size-4" />
            </button>
          </>
        ) : (
          // Collapsed: the icon doubles as the expand affordance — saves a
          // row and is the most discoverable place to put the toggle when
          // there's no room for a second button.
          <button
            onClick={toggle}
            aria-label="Expand sidebar"
            title="TokenScope — click to expand"
            className="flex size-8 items-center justify-center rounded-md text-foreground transition-colors hover:bg-muted"
          >
            <Logo variant="icon" className="size-5" />
          </button>
        )}
      </div>

      <nav className="flex flex-1 flex-col gap-1 p-1.5">
        {navItems.map((item) => (
          <NavLink
            key={item.to}
            to={`${item.to}${toolbarSearch}`}
            end={item.to === "/"}
            className={({ isActive }) =>
              cn(
                "group relative flex items-center gap-3 rounded-md px-2.5 py-2 text-sm font-medium transition-colors",
                isActive
                  ? "bg-muted text-foreground"
                  : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
                !expanded && "justify-center px-0",
              )
            }
          >
            <item.icon className="size-4 shrink-0" />
            {expanded && <span>{item.label}</span>}
            {!expanded && (
              <span className="pointer-events-none absolute left-full ml-2 hidden rounded-md bg-foreground px-2 py-1 text-xs text-background group-hover:block">
                {item.label}
              </span>
            )}
          </NavLink>
        ))}
      </nav>
    </aside>
  )
}
