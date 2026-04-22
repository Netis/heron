import { useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { Markdown } from "@/components/ui/markdown"
import { segmentClaudeCliUserText } from "./claude-cli-segment"
import type { CallOverlay } from "./types"

/**
 * claude-cli overlay — Claude Code embeds agent-scaffold artifacts in
 * user-message text: <system-reminder>, <command-name>/-message/-args
 * triples (for slash commands), <local-command-stdout> for their output.
 *
 * When this overlay is active, user-message text is segmented and the
 * scaffold blocks are collapsed by default, revealing the real user
 * input as the main content.
 */
export const ClaudeCliOverlay: CallOverlay = {
  UserMessageContent: ClaudeCliUserMessage,
}

// ── segment renderers ──────────────────────────────────────────────────────

function SystemReminderFold({ body }: { body: string }) {
  const [open, setOpen] = useState(false)
  const lines = body.split("\n").length
  return (
    <div className="rounded border border-amber-200 bg-amber-50/60 dark:border-amber-900/40 dark:bg-amber-900/10 text-[11px]">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-1 px-2 py-1 text-left text-[10px] text-amber-800 dark:text-amber-300">
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>system-reminder</span>
        <span className="text-amber-700/70 dark:text-amber-300/70">({lines} line{lines === 1 ? "" : "s"})</span>
      </button>
      {open && (
        <pre className="max-h-[300px] overflow-auto whitespace-pre-wrap px-3 pb-2 font-sans text-[11px] text-muted-foreground">
          {body}
        </pre>
      )}
    </div>
  )
}

function CommandFold({ name, message, args }: { name: string; message: string; args: string }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded border border-sky-200 bg-sky-50/60 dark:border-sky-900/40 dark:bg-sky-900/10 text-[11px]">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-1 px-2 py-1 text-left text-[10px] text-sky-800 dark:text-sky-300">
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span className="font-mono">/{name}</span>
        {message && <span className="truncate text-sky-700/70 dark:text-sky-300/70">{message}</span>}
      </button>
      {open && (
        <div className="space-y-1 px-3 pb-2">
          {message && (
            <div>
              <div className="text-[9px] uppercase text-sky-700/80 dark:text-sky-300/80">message</div>
              <pre className="whitespace-pre-wrap font-sans text-[11px]">{message}</pre>
            </div>
          )}
          {args && (
            <div>
              <div className="text-[9px] uppercase text-sky-700/80 dark:text-sky-300/80">args</div>
              <pre className="max-h-[200px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{args}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function LocalCommandStdoutFold({ body }: { body: string }) {
  const [open, setOpen] = useState(false)
  const lines = body.split("\n").length
  return (
    <div className="rounded border border-muted bg-muted/20 text-[11px]">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-1 px-2 py-1 text-left text-[10px] text-muted-foreground">
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>command output</span>
        <span>({lines} line{lines === 1 ? "" : "s"})</span>
      </button>
      {open && (
        <pre className="max-h-[300px] overflow-auto whitespace-pre-wrap px-3 pb-2 font-mono text-[10px]">
          {body}
        </pre>
      )}
    </div>
  )
}

function ClaudeCliUserMessage({ text }: { text: string }) {
  const segs = segmentClaudeCliUserText(text)
  if (segs.length === 0) return null
  return (
    <div className="space-y-1">
      {segs.map((s, i) => {
        switch (s.kind) {
          case "plain": {
            const trimmed = s.text.trim()
            if (!trimmed) return null
            return (
              <div key={i} className="text-[11px]">
                <Markdown text={s.text} />
              </div>
            )
          }
          case "system-reminder":
            return <SystemReminderFold key={i} body={s.body} />
          case "command":
            return <CommandFold key={i} name={s.name} message={s.message} args={s.args} />
          case "local-command-stdout":
            return <LocalCommandStdoutFold key={i} body={s.body} />
        }
      })}
    </div>
  )
}
