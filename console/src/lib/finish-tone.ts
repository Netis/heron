export type FinishTone = "ok" | "warn" | "tool" | "pause" | "err" | "muted"

const TONE: Record<string, FinishTone> = {
  // Natural completion
  end_turn: "ok",
  stop: "ok",
  STOP: "ok",
  stop_sequence: "ok",
  completed: "ok",
  // Truncation
  max_tokens: "warn",
  length: "warn",
  MAX_TOKENS: "warn",
  model_context_window_exceeded: "warn",
  incomplete: "warn",
  // Tool use
  tool_use: "tool",
  tool_calls: "tool",
  function_call: "tool",
  TOOL_CALLS: "tool",
  // Server-tool yield
  pause_turn: "pause",
  // Safety / failure
  refusal: "err",
  content_filter: "err",
  SAFETY: "err",
  RECITATION: "err",
  failed: "err",
  cancelled: "err",
}

export function finishTone(reason: string | null | undefined): FinishTone {
  if (!reason) return "muted"
  return TONE[reason] ?? "muted"
}

export const TONE_CLASS: Record<FinishTone, string> = {
  ok: "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400",
  warn: "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400",
  tool: "bg-blue-100 text-blue-700 dark:bg-blue-900/30 dark:text-blue-400",
  pause: "bg-sky-100 text-sky-700 dark:bg-sky-900/30 dark:text-sky-400",
  err: "bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-400",
  muted: "bg-gray-100 text-gray-600 dark:bg-gray-800/30 dark:text-gray-400",
}
