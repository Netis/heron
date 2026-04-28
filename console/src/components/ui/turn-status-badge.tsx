import { cn } from "@/lib/utils"

const colorMap: Record<string, string> = {
  complete: "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400",
  incomplete: "bg-gray-100 text-gray-600 dark:bg-gray-800/30 dark:text-gray-400",
}

export function TurnStatusBadge({ status }: { status: string | null }) {
  if (!status) return <span className="text-muted-foreground">—</span>

  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-xs font-medium",
        colorMap[status] ?? "bg-gray-100 text-gray-600 dark:bg-gray-800/30 dark:text-gray-400",
      )}
    >
      {status}
    </span>
  )
}
