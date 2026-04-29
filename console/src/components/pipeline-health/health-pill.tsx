import { cn } from "@/lib/utils"
import type { HealthLevel } from "@/lib/pipeline-health"

type Props = { level: HealthLevel; count?: number }

const LABELS: Record<HealthLevel, string> = {
  healthy: "Healthy",
  warning: "Warning",
  critical: "Critical",
}

const STYLES: Record<HealthLevel, string> = {
  healthy:
    "bg-emerald-100 text-emerald-700 dark:bg-emerald-950 dark:text-emerald-300",
  warning:
    "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300",
  critical: "bg-red-100 text-red-700 dark:bg-red-950 dark:text-red-300",
}

export function HealthPill({ level, count }: Props) {
  const label =
    level === "healthy" || count === undefined || count === 0
      ? LABELS[level]
      : `${count} ${level === "critical" ? "critical" : "warnings"}`
  return (
    <span
      className={cn(
        "inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium",
        STYLES[level],
      )}
    >
      {level === "critical" && "✗ "}
      {level === "warning" && "⚠ "}
      {label}
    </span>
  )
}
