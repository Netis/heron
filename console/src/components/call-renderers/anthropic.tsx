import { useMemo, useState } from "react"
import { ChevronRight, ChevronDown, Zap } from "lucide-react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { parseAnthropicCall } from "@/lib/wire-apis/anthropic"
import type {
  AnthropicBlock,
  AnthropicMessage,
  AnthropicRequest,
  AnthropicResponse,
} from "@/lib/wire-apis/anthropic/types"
import type { CallOverlay } from "./overlays/types"
import { ToolUsePointer, ToolResultBackLink } from "@/components/turn-detail/tool-pointer"
import { classifyToolUseState, classifyToolResultState, type ToolIndex, type TurnForClassification } from "@/lib/turn-index"

// ── helpers ────────────────────────────────────────────────────────────────

function formatJson(v: unknown): string {
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

function byteLength(s: string): number {
  return new Blob([s]).size
}

// ── ctx types ──────────────────────────────────────────────────────────────

interface OutputCtx {
  toolIndex: ToolIndex
  callId: string
  isFinalCall: boolean
  turn: TurnForClassification
}

interface InputCtx {
  toolIndex: ToolIndex
}

// ── block sub-renderers ────────────────────────────────────────────────────

function TextBlockView({ text, renderUserMessage }: { text: string; renderUserMessage?: (t: string) => React.ReactNode }) {
  return (
    <div className="text-[11px]">
      {renderUserMessage ? renderUserMessage(text) : <Markdown text={text} />}
    </div>
  )
}

function ToolUseBlockView({
  id,
  name,
  input,
  ctx,
}: {
  id: string
  name: string
  input: unknown
  ctx?: OutputCtx
}) {
  const [argsOpen, setArgsOpen] = useState(true)
  const entry = ctx?.toolIndex.get(id) ?? { origin: null, resolution: null }
  const state = ctx ? classifyToolUseState(entry, { isFinalCall: ctx.isFinalCall, turn: ctx.turn }) : "healthy"
  return (
    <div className="rounded bg-amber-50/60 border border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40 p-2 text-[11px]">
      <div className="flex items-center gap-2">
        <span className="font-medium">🔧 {name}</span>
        <span className="font-mono text-[10px] text-muted-foreground">{id}</span>
      </div>
      <details className="mt-1" open={argsOpen} onToggle={(e) => setArgsOpen((e.target as HTMLDetailsElement).open)}>
        <summary className="cursor-pointer text-muted-foreground text-[10px]">input</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
          {formatJson(input)}
        </pre>
      </details>
      {ctx && (
        <div className="mt-1">
          <ToolUsePointer state={state} resolution={entry.resolution} />
        </div>
      )}
    </div>
  )
}

function ToolResultBlockView({
  toolUseId,
  content,
  isError,
  renderToolResult,
}: {
  toolUseId: string
  content: string | Array<{ type: string; [k: string]: unknown }>
  isError: boolean
  renderToolResult?: (content: string, isError: boolean) => React.ReactNode
}) {
  const contentStr = typeof content === "string" ? content : formatJson(content)
  const rendered = renderToolResult ? renderToolResult(contentStr, isError) : (
    <pre
      className={cn(
        "max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]",
        isError && "text-red-600",
      )}
    >
      {contentStr}
    </pre>
  )
  return (
    <div className="rounded bg-muted/40 p-2 text-[11px]">
      <div className={cn("mb-1", isError && "text-red-600")}>
        ⤷ {isError ? "error" : "result"} · {formatSize(byteLength(contentStr))} ·{" "}
        <span className="font-mono text-muted-foreground">{toolUseId}</span>
      </div>
      {rendered}
    </div>
  )
}

function ImageBlockView({ block }: { block: Extract<AnthropicBlock, { type: "image" }> }) {
  if (block.source.type === "base64") {
    const src = `data:${block.source.media_type};base64,${block.source.data}`
    return (
      <div className="flex items-start gap-2 text-[11px]">
        <span className="text-muted-foreground">🖼️ image ({block.source.media_type})</span>
        <img src={src} alt="" className="max-h-40 max-w-xs rounded border border-border" />
      </div>
    )
  }
  return (
    <div className="text-[11px] text-muted-foreground">
      🖼️ image · <a href={block.source.url} className="font-mono hover:underline" target="_blank" rel="noreferrer">{block.source.url}</a>
    </div>
  )
}

function ThinkingBlockView({ block }: { block: Extract<AnthropicBlock, { type: "thinking" }> }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded bg-purple-50/60 border border-purple-200 dark:bg-purple-900/10 dark:border-purple-900/40 p-2 text-[11px]">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 text-left"
      >
        {open ? <ChevronDown className="size-3 text-purple-700 dark:text-purple-400" /> : <ChevronRight className="size-3 text-purple-700 dark:text-purple-400" />}
        <span className="font-medium text-purple-800 dark:text-purple-300">💭 thinking</span>
        <span className="text-[10px] text-muted-foreground">{formatSize(byteLength(block.thinking))}</span>
        {block.signature && (
          <span className="ml-auto font-mono text-[9px] text-muted-foreground" title="signature">
            sig: {block.signature.slice(0, 8)}…
          </span>
        )}
      </button>
      {open && (
        <pre className="mt-2 max-h-[400px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">
          {block.thinking}
        </pre>
      )}
    </div>
  )
}

function BlockView({
  block,
  ctx,
  overlay,
}: {
  block: AnthropicBlock
  ctx?: OutputCtx
  overlay?: CallOverlay | null
  isUserMessage?: boolean
}) {
  const UserMsg = overlay?.UserMessageContent
  const ToolResult = overlay?.ToolResultContent
  switch (block.type) {
    case "text":
      return <TextBlockView text={block.text} renderUserMessage={UserMsg ? (t) => <UserMsg text={t} /> : undefined} />
    case "tool_use":
      return <ToolUseBlockView id={block.id} name={block.name} input={block.input} ctx={ctx} />
    case "tool_result":
      return (
        <ToolResultBlockView
          toolUseId={block.tool_use_id}
          content={block.content}
          isError={block.is_error}
          renderToolResult={ToolResult ? (c, e) => <ToolResult content={c} isError={e} /> : undefined}
        />
      )
    case "image":
      return <ImageBlockView block={block} />
    case "document":
      return (
        <div className="text-[11px] text-muted-foreground">
          📄 document{block.title ? ` — ${block.title}` : ""}
        </div>
      )
    case "thinking":
      return <ThinkingBlockView block={block} />
    case "redacted_thinking":
      return (
        <div className="rounded bg-purple-50/40 border border-purple-200/50 p-2 text-[11px] text-purple-700 italic">
          💭 redacted thinking ({formatSize(byteLength(block.data))})
        </div>
      )
    case "unknown":
      return (
        <details className="text-[11px]">
          <summary className="cursor-pointer text-red-600">⚠️ unknown block</summary>
          <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
            {formatJson(block.raw)}
          </pre>
        </details>
      )
  }
}

// ── message list ───────────────────────────────────────────────────────────

const ROLE_STYLES: Record<string, string> = {
  user: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  assistant: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
}

function messagePreview(msg: AnthropicMessage): string {
  for (const b of msg.content) {
    if (b.type === "text") return b.text.slice(0, 120)
    if (b.type === "tool_use") return `🔧 ${b.name}(${formatJson(b.input).slice(0, 60)})`
    if (b.type === "tool_result") {
      const size = typeof b.content === "string" ? byteLength(b.content) : 0
      return `⤷ ${b.tool_use_id} · ${formatSize(size)}`
    }
    if (b.type === "thinking") return `💭 ${b.thinking.slice(0, 120)}`
    if (b.type === "image") return `🖼️ image`
  }
  return ""
}

function MessageRow({
  msg,
  index,
  ctx,
  overlay,
}: {
  msg: AnthropicMessage
  index: number
  ctx?: OutputCtx
  overlay?: CallOverlay | null
}) {
  const [open, setOpen] = useState(false)
  const preview = messagePreview(msg)
  const isOnlyToolResult = msg.role === "user" && msg.content.length > 0 && msg.content.every((b) => b.type === "tool_result")
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        <span className="w-5 shrink-0 text-[10px] tabular-nums text-muted-foreground">#{index + 1}</span>
        <span className={cn("shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium", isOnlyToolResult ? "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300" : ROLE_STYLES[msg.role])}>
          {isOnlyToolResult ? "tool" : msg.role}
        </span>
        <span className="flex-1 truncate text-muted-foreground">{preview}</span>
        {open ? <ChevronDown className="size-3 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3 shrink-0 text-muted-foreground" />}
      </button>
      {open && (
        <div className="space-y-2 border-t border-border/30 bg-muted/10 px-3 py-2">
          {msg.content.map((b, i) => (
            <BlockView key={i} block={b} ctx={ctx} overlay={overlay} isUserMessage={msg.role === "user"} />
          ))}
        </div>
      )}
    </div>
  )
}

function MessagesSection({
  messages,
  ctx,
  overlay,
}: {
  messages: AnthropicMessage[]
  ctx?: OutputCtx
  overlay?: CallOverlay | null
}) {
  const [open, setOpen] = useState(true)
  if (messages.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Messages</span>
        <span className="text-muted-foreground">({messages.length})</span>
      </button>
      {open && <div>{messages.map((m, i) => <MessageRow key={i} msg={m} index={i} ctx={ctx} overlay={overlay} />)}</div>}
    </div>
  )
}

// ── system / tools / sampling ──────────────────────────────────────────────

function SystemSection({ request }: { request: AnthropicRequest }) {
  const [open, setOpen] = useState(false)
  const sys = request.system
  if (!sys) return null
  let text = ""
  let segmentCount = 1
  let cacheSegments = 0
  if (sys.kind === "string") {
    text = sys.text
  } else {
    text = sys.blocks.map((b) => b.text).join("\n\n")
    segmentCount = sys.blocks.length
    cacheSegments = sys.blocks.filter((b) => b.cache_control).length
  }
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">System</span>
        <span className="text-muted-foreground">· {formatSize(byteLength(text))}</span>
        {segmentCount > 1 && <span className="text-muted-foreground">· {segmentCount} segments</span>}
        {cacheSegments > 0 && (
          <span className="inline-flex items-center gap-0.5 rounded bg-purple-100 px-1.5 py-0.5 text-[10px] text-purple-700 dark:bg-purple-900/40 dark:text-purple-300">
            <Zap className="size-2.5" /> {cacheSegments} cached
          </span>
        )}
      </button>
      {open && <div className="px-3 py-2 text-[11px]"><Markdown text={text} /></div>}
    </div>
  )
}

function ToolsSection({ request }: { request: AnthropicRequest }) {
  const [open, setOpen] = useState(false)
  if (request.tools.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Tools</span>
        <span className="text-muted-foreground">({request.tools.length})</span>
      </button>
      {open && (
        <div className="space-y-2 px-3 py-2">
          {request.tools.map((t, i) => (
            <div key={i} className="rounded border border-border/40 bg-muted/20 p-2 text-[11px]">
              <div className="font-medium">
                {t.name}
                {t.cache_control && (
                  <span className="ml-2 inline-flex items-center gap-0.5 rounded bg-purple-100 px-1 py-0.5 text-[9px] text-purple-700 dark:bg-purple-900/40 dark:text-purple-300">
                    <Zap className="size-2" />
                  </span>
                )}
              </div>
              {t.description && <div className="text-muted-foreground">{t.description}</div>}
              <details className="mt-1">
                <summary className="cursor-pointer text-[10px] text-muted-foreground">input_schema</summary>
                <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
                  {formatJson(t.input_schema)}
                </pre>
              </details>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function SamplingSection({ request }: { request: AnthropicRequest }) {
  const [open, setOpen] = useState(false)
  const s = request.sampling
  const rows: Array<[string, string | number | null]> = [
    ["model", request.model],
    ["max_tokens", s.max_tokens],
    ["temperature", s.temperature],
    ["top_p", s.top_p],
    ["top_k", s.top_k],
    ["stream", s.stream == null ? null : s.stream ? "true" : "false"],
    ["stop_sequences", s.stop_sequences.length ? s.stop_sequences.join(", ") : null],
    ["tool_choice", s.tool_choice !== undefined ? formatJson(s.tool_choice) : null],
    ["user_id", s.user_id],
  ]
  const visible = rows.filter(([, v]) => v != null && v !== "")
  if (visible.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Sampling</span>
      </button>
      {open && (
        <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 px-3 py-2 text-[11px]">
          {visible.map(([k, v]) => (
            <div key={k} className="contents">
              <span className="text-muted-foreground">{k}</span>
              <span className="truncate font-mono" title={String(v)}>{String(v)}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

// ── response side ──────────────────────────────────────────────────────────

function StopReasonBadge({ reason }: { reason: string | null }) {
  if (!reason) return null
  const styles: Record<string, string> = {
    end_turn: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
    tool_use: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
    max_tokens: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    stop_sequence: "bg-muted text-muted-foreground",
    pause_turn: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
    refusal: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
  }
  return (
    <span className={cn("rounded px-1.5 py-0.5 text-[10px] font-medium", styles[reason] ?? "bg-muted text-muted-foreground")}>
      {reason}
    </span>
  )
}

function UsageCard({ response }: { response: AnthropicResponse }) {
  const u = response.usage
  const hasCache = (u.cache_read_input_tokens ?? 0) > 0 || (u.cache_creation_input_tokens ?? 0) > 0
  const totalInput = (u.input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0)
  const cacheHitRatio = hasCache && totalInput > 0 ? ((u.cache_read_input_tokens ?? 0) / totalInput) * 100 : null
  return (
    <div className="rounded border border-border/60 bg-background p-3 text-[11px]">
      <div className="mb-1 flex items-center gap-2">
        <span className="font-medium">Usage</span>
        <StopReasonBadge reason={typeof response.stop_reason === "string" ? response.stop_reason : null} />
        {response.stop_sequence && <span className="font-mono text-[10px] text-muted-foreground">seq: {response.stop_sequence}</span>}
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5">
        <span className="text-muted-foreground">input</span>
        <span className="tabular-nums">{u.input_tokens ?? "—"}</span>
        <span className="text-muted-foreground">output</span>
        <span className="tabular-nums">{u.output_tokens ?? "—"}</span>
        {u.cache_read_input_tokens != null && (
          <>
            <span className="text-muted-foreground">cache_read</span>
            <span className="tabular-nums text-purple-700 dark:text-purple-300">{u.cache_read_input_tokens}</span>
          </>
        )}
        {u.cache_creation_input_tokens != null && (
          <>
            <span className="text-muted-foreground">cache_creation</span>
            <span className="tabular-nums text-purple-700 dark:text-purple-300">{u.cache_creation_input_tokens}</span>
          </>
        )}
        {cacheHitRatio != null && (
          <>
            <span className="text-muted-foreground">cache hit</span>
            <span className="tabular-nums">{cacheHitRatio.toFixed(0)}%</span>
          </>
        )}
      </div>
    </div>
  )
}

// ── exported views ─────────────────────────────────────────────────────────

export interface AnthropicCallViewProps {
  requestBody: string | null
  responseBody: string | null
  overlay?: CallOverlay | null
  hasRequestBody: boolean
}

export function AnthropicCallView({
  requestBody,
  responseBody,
  overlay,
  hasRequestBody,
}: AnthropicCallViewProps) {
  const call = useMemo(() => parseAnthropicCall(requestBody, responseBody), [requestBody, responseBody])

  return (
    <>
      <section className="border-l-2 border-muted-foreground/30 pl-3">
        <div className="mb-2 flex items-center gap-3 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
          <span>Input</span>
          {call.request.cache_control_count > 0 && (
            <span className="inline-flex items-center gap-0.5 rounded bg-purple-100 px-1.5 py-0.5 text-[10px] normal-case text-purple-700 dark:bg-purple-900/40 dark:text-purple-300">
              <Zap className="size-2.5" /> {call.request.cache_control_count} cache marker{call.request.cache_control_count === 1 ? "" : "s"}
            </span>
          )}
        </div>
        {!hasRequestBody ? (
          <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
            Request body not captured.
          </div>
        ) : (
          <div className="space-y-2">
            <SystemSection request={call.request} />
            <MessagesSection messages={call.request.messages} overlay={overlay} />
            <ToolsSection request={call.request} />
            <SamplingSection request={call.request} />
          </div>
        )}
      </section>
      <section className="border-l-2 border-emerald-500/40 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
          Output
        </div>
        <AnthropicOutputBlocks
          response={call.response}
          overlay={overlay}
        />
        <div className="mt-2">
          <UsageCard response={call.response} />
        </div>
      </section>
    </>
  )
}

/**
 * Output-only variant used by the turn-detail CallCard expanded state.
 * Same block rendering as the full view's output section, without the input
 * side and without its own usage card (the card card already displays tokens).
 */
export function AnthropicOutputBlocks({
  response,
  ctx,
  overlay,
}: {
  response: AnthropicResponse
  ctx?: OutputCtx
  overlay?: CallOverlay | null
}) {
  if (response.content.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response content.</div>
  }
  return (
    <div className="space-y-2">
      {response.content.map((b, i) => (
        <BlockView key={i} block={b} ctx={ctx} overlay={overlay} />
      ))}
    </div>
  )
}

/**
 * Helper for CallCard: parse + extract output in one call.
 */
export function anthropicParseForOutput(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
) {
  const call = parseAnthropicCall(requestBody, responseBody)
  return { response: call.response }
}

// ── Input subsection (tool_result back-pointers + optional user text) ──────

export interface AnthropicParsedInput {
  toolResults: Array<{
    tool_use_id: string
    content: string
    is_error: boolean
  }>
  extraUserText: string | null
}

// eslint-disable-next-line react-refresh/only-export-components
export function anthropicParseForInput(requestBody: string | null | undefined): AnthropicParsedInput {
  if (!requestBody) return { toolResults: [], extraUserText: null }
  const call = parseAnthropicCall(requestBody, null)
  // Take only the last user-role message's content — that's the delta from the prior call.
  const lastUserMsg = [...call.request.messages].reverse().find((m) => m.role === "user")
  if (!lastUserMsg) return { toolResults: [], extraUserText: null }
  const toolResults: AnthropicParsedInput["toolResults"] = []
  let extraUserText: string | null = null
  for (const block of lastUserMsg.content) {
    if (block.type === "tool_result") {
      const content = typeof block.content === "string" ? block.content : formatJson(block.content)
      toolResults.push({ tool_use_id: block.tool_use_id, content, is_error: block.is_error })
    } else if (block.type === "text") {
      extraUserText = (extraUserText ?? "") + (extraUserText ? "\n\n" : "") + block.text
    }
  }
  return { toolResults, extraUserText }
}

export function AnthropicInputBlocks({
  parsed,
  ctx,
  overlay,
}: {
  parsed: AnthropicParsedInput
  ctx: InputCtx
  overlay?: CallOverlay | null
}) {
  const ToolResult = overlay?.ToolResultContent
  if (parsed.toolResults.length === 0 && !parsed.extraUserText) {
    return <div className="text-[11px] text-muted-foreground italic">No input deltas.</div>
  }
  return (
    <div className="space-y-2">
      {parsed.toolResults.map((tr) => {
        const entry = ctx.toolIndex.get(tr.tool_use_id) ?? { origin: null, resolution: null }
        const state = classifyToolResultState(entry)
        const errored = tr.is_error
        return (
          <div
            key={tr.tool_use_id}
            className={cn(
              "rounded border p-2 text-[11px]",
              errored
                ? "bg-red-50 border-red-200 dark:bg-red-900/10 dark:border-red-900/40"
                : state === "orphan"
                  ? "bg-amber-50/60 border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40"
                  : "bg-muted/40 border-border/60",
            )}
          >
            <div className="flex items-center gap-2">
              <span className={cn("font-medium", errored && "text-red-700 dark:text-red-400")}>
                ⤷ {errored ? "error" : "tool_result"}
              </span>
              <span className="font-mono text-[10px] text-muted-foreground">{tr.tool_use_id}</span>
              <span className="text-[10px] text-muted-foreground">· {formatSize(byteLength(tr.content))}</span>
            </div>
            <div className="mt-1">
              {ToolResult
                ? <ToolResult content={tr.content} isError={errored} />
                : <pre className={cn("max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]", errored && "text-red-700 dark:text-red-400")}>{tr.content}</pre>}
            </div>
            <div className="mt-1">
              <ToolResultBackLink state={state} origin={entry.origin} />
            </div>
          </div>
        )
      })}
      {parsed.extraUserText && (
        <div className="rounded border border-blue-200 bg-blue-50/60 p-3 text-[11px] dark:border-blue-900/40 dark:bg-blue-900/10">
          <Markdown text={parsed.extraUserText} />
        </div>
      )}
    </div>
  )
}
