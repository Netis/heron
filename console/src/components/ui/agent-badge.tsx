import { cn } from "@/lib/utils"

const COLOUR_BY_AGENT: Record<string, string> = {
  "claude-cli": "bg-emerald-500/20 text-emerald-700 dark:text-emerald-300 border-emerald-700/30",
  "codex-cli": "bg-orange-500/20 text-orange-700 dark:text-orange-300 border-orange-700/30",
  openclaw: "bg-sky-500/20 text-sky-700 dark:text-sky-300 border-sky-700/30",
  hermes: "bg-violet-500/20 text-violet-700 dark:text-violet-300 border-violet-700/30",
}

export function AgentBadge({ agentKind }: { agentKind: string }) {
  const palette = COLOUR_BY_AGENT[agentKind] ?? "bg-muted text-muted-foreground border-border"
  return (
    <span
      className={cn(
        "inline-flex items-center rounded border px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide",
        palette,
      )}
    >
      {agentKind}
    </span>
  )
}
