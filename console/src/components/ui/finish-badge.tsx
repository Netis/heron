import { cn } from "@/lib/utils"

const colorMap: Record<string, string> = {
  complete: "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400",
  length: "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400",
  tool_use: "bg-blue-100 text-blue-700 dark:bg-blue-900/30 dark:text-blue-400",
  error: "bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-400",
  cancelled: "bg-gray-100 text-gray-600 dark:bg-gray-800/30 dark:text-gray-400",
}

export function FinishBadge({ reason }: { reason: string | null }) {
  if (!reason) return <span className="text-muted-foreground">—</span>

  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-xs font-medium",
        colorMap[reason] ?? "bg-gray-100 text-gray-600",
      )}
    >
      {reason}
    </span>
  )
}
