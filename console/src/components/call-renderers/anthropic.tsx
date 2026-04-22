import { useMemo, useState } from "react"
import { ChevronRight, ChevronDown, Zap } from "lucide-react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { parseAnthropicCall } from "@/lib/wire-apis/anthropic"
import type {
  AnthropicBlock,
  AnthropicCall,
  AnthropicMessage,
  AnthropicRequest,
  AnthropicResponse,
} from "@/lib/wire-apis/anthropic/types"
import type { CallOverlay } from "./overlays/types"

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

// ── block sub-renderers ────────────────────────────────────────────────────

interface ToolResultLookup {
  (toolUseId: string): { content: string; is_error: boolean } | null
}

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
  resultLookup,
}: {
  id: string
  name: string
  input: unknown
  resultLookup?: ToolResultLookup
}) {
  const [argsOpen, setArgsOpen] = useState(true)
  const [resultOpen, setResultOpen] = useState(false)
  const result = resultLookup ? resultLookup(id) : null
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
      {result ? (
        <details className="mt-1" open={resultOpen} onToggle={(e) => setResultOpen((e.target as HTMLDetailsElement).open)}>
          <summary className={cn("cursor-pointer text-[10px]", result.is_error ? "text-red-600" : "text-muted-foreground")}>
            ⤷ {result.is_error ? "error" : "result"} · {formatSize(byteLength(result.content))}
          </summary>
          <pre
            className={cn(
              "mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]",
              result.is_error && "text-red-600",
            )}
          >
            {result.content}
          </pre>
        </details>
      ) : resultLookup ? (
        <div className="mt-1 text-[10px] text-muted-foreground italic">⤷ result · (no response, turn ended)</div>
      ) : null}
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
  resultLookup,
  overlay,
}: {
  block: AnthropicBlock
  resultLookup?: ToolResultLookup
  overlay?: CallOverlay | null
  isUserMessage?: boolean
}) {
  const UserMsg = overlay?.UserMessageContent
  const ToolResult = overlay?.ToolResultContent
  switch (block.type) {
    case "text":
      return <TextBlockView text={block.text} renderUserMessage={UserMsg ? (t) => <UserMsg text={t} /> : undefined} />
    case "tool_use":
      return <ToolUseBlockView id={block.id} name={block.name} input={block.input} resultLookup={resultLookup} />
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
  resultLookup,
  overlay,
}: {
  msg: AnthropicMessage
  index: number
  resultLookup?: ToolResultLookup
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
            <BlockView key={i} block={b} resultLookup={resultLookup} overlay={overlay} isUserMessage={msg.role === "user"} />
          ))}
        </div>
      )}
    </div>
  )
}

function MessagesSection({
  messages,
  resultLookup,
  overlay,
}: {
  messages: AnthropicMessage[]
  resultLookup?: ToolResultLookup
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
      {open && <div>{messages.map((m, i) => <MessageRow key={i} msg={m} index={i} resultLookup={resultLookup} overlay={overlay} />)}</div>}
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

// ── tool-result lookup helper ──────────────────────────────────────────────

function buildResultLookup(_call: AnthropicCall, nextCallRequestBody: string | null | undefined): ToolResultLookup | undefined {
  if (!nextCallRequestBody) return undefined
  // Parse the next call's request to extract tool_result blocks.
  const next = parseAnthropicCall(nextCallRequestBody, null)
  const map = new Map<string, { content: string; is_error: boolean }>()
  for (const msg of next.request.messages) {
    for (const block of msg.content) {
      if (block.type === "tool_result") {
        const c = typeof block.content === "string" ? block.content : formatJson(block.content)
        map.set(block.tool_use_id, { content: c, is_error: block.is_error })
      }
    }
  }
  return (id: string) => map.get(id) ?? null
}

// ── exported views ─────────────────────────────────────────────────────────

export interface AnthropicCallViewProps {
  requestBody: string | null
  responseBody: string | null
  nextCallRequestBody?: string | null
  overlay?: CallOverlay | null
  hasRequestBody: boolean
  onOpenRawHttp: () => void
}

export function AnthropicCallView({
  requestBody,
  responseBody,
  nextCallRequestBody,
  overlay,
  hasRequestBody,
  onOpenRawHttp,
}: AnthropicCallViewProps) {
  const call = useMemo(() => parseAnthropicCall(requestBody, responseBody), [requestBody, responseBody])
  const resultLookup = useMemo(() => buildResultLookup(call, nextCallRequestBody), [call, nextCallRequestBody])

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
            <div className="flex justify-end">
              <button onClick={onOpenRawHttp} className="text-[10px] text-muted-foreground hover:text-foreground hover:underline">
                View raw HTTP →
              </button>
            </div>
          </div>
        )}
      </section>
      <section className="border-l-2 border-emerald-500/40 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
          Output
        </div>
        <AnthropicOutputBlocks
          response={call.response}
          resultLookup={resultLookup}
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
  resultLookup,
  overlay,
}: {
  response: AnthropicResponse
  resultLookup?: ToolResultLookup
  overlay?: CallOverlay | null
}) {
  if (response.content.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response content.</div>
  }
  return (
    <div className="space-y-2">
      {response.content.map((b, i) => (
        <BlockView key={i} block={b} resultLookup={resultLookup} overlay={overlay} />
      ))}
    </div>
  )
}

/**
 * Helper for CallCard: parse + extract output + build result lookup in one call.
 */
export function anthropicParseForOutput(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
  nextCallRequestBody: string | null | undefined,
) {
  const call = parseAnthropicCall(requestBody, responseBody)
  const resultLookup = buildResultLookup(call, nextCallRequestBody)
  return { response: call.response, resultLookup }
}
