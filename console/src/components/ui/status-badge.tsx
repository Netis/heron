import { cn } from "@/lib/utils"

export function StatusBadge({ status }: { status: number | null }) {
  if (status == null) return <span className="text-muted-foreground">—</span>

  const color =
    status >= 500
      ? "bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-400"
      : status === 429
        ? "bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-400"
        : status >= 400
          ? "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400"
          : "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400"

  return (
    <span
      className={cn(
        "inline-flex items-center justify-center rounded px-1.5 py-0.5 text-xs font-medium tabular-nums",
        color,
      )}
    >
      {status}
    </span>
  )
}
