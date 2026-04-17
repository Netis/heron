import { Outlet } from "react-router"
import { Sidebar } from "./sidebar"
import { Toolbar } from "./toolbar"
import { useSidebarStore } from "@/stores/sidebar"
import { useAutoRefresh } from "@/hooks/use-auto-refresh"
import { useToolbarUrlSync } from "@/hooks/use-url-sync"
import { cn } from "@/lib/utils"

export function AppLayout() {
  const expanded = useSidebarStore((s) => s.expanded)
  useAutoRefresh()
  useToolbarUrlSync()

  return (
    <div className="flex h-screen overflow-hidden">
      <Sidebar />
      <div
        className={cn(
          "flex flex-1 flex-col transition-[margin-left] duration-200",
          expanded ? "ml-[200px]" : "ml-[44px]",
        )}
      >
        <Toolbar />
        <main className="flex-1 overflow-auto">
          <Outlet />
        </main>
      </div>
    </div>
  )
}
