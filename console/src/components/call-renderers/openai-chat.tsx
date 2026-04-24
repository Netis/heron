import { useMemo, useState } from "react"
import { ChevronRight, ChevronDown, FileJson, Activity } from "lucide-react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { parseOpenAiChatCall } from "@/lib/wire-apis/openai-chat"
import type {
  OpenAiChatChoice,
  OpenAiChatMessage,
  OpenAiChatMessageContent,
  OpenAiChatPart,
  OpenAiChatRequest,
  OpenAiChatResponse,
  OpenAiChatResponseFormat,
  OpenAiChatToolCall,
} from "@/lib/wire-apis/openai-chat/types"
import type { CallOverlay } from "./overlays/types"

// ── helpers ────────────────────────────────────────────────────────────────

function formatJson(v: unknown): string {
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}

function byteLength(s: string): number {
  return new Blob([s]).size
}

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

// ── content parts ──────────────────────────────────────────────────────────

function ContentPartView({ part }: { part: OpenAiChatPart }) {
  switch (part.type) {
    case "text":
      return (
        <div className="text-[11px]">
          <Markdown text={part.text} />
        </div>
      )
    case "image_url": {
      const url = part.image_url.url
      const isDataUri = url.startsWith("data:")
      return (
        <div className="flex items-start gap-2 text-[11px]">
          <span className="text-muted-foreground">🖼️ image</span>
          {isDataUri ? (
            <img src={url} alt="" className="max-h-40 max-w-xs rounded border border-border" />
          ) : (
            <a href={url} className="font-mono hover:underline" target="_blank" rel="noreferrer">
              {url}
            </a>
          )}
          {part.image_url.detail && <span className="text-muted-foreground">({part.image_url.detail})</span>}
        </div>
      )
    }
    case "input_audio":
      return (
        <div className="text-[11px] text-muted-foreground">
          🎵 audio ({part.input_audio.format}) · {formatSize(byteLength(part.input_audio.data))}
        </div>
      )
    case "unknown":
      return (
        <details className="text-[11px]">
          <summary className="cursor-pointer text-red-600">⚠️ unknown part</summary>
          <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
            {formatJson(part.raw)}
          </pre>
        </details>
      )
  }
}

function MessageContent({
  content,
  isUserText,
  overlay,
}: {
  content: OpenAiChatMessageContent
  isUserText: boolean
  overlay?: CallOverlay | null
}) {
  if (content == null) return null
  if (typeof content === "string") {
    const UserMsg = overlay?.UserMessageContent
    if (isUserText && UserMsg) return <UserMsg text={content} />
    return (
      <div className="text-[11px]">
        <Markdown text={content} />
      </div>
    )
  }
  return (
    <div className="space-y-1">
      {content.map((p, i) => <ContentPartView key={i} part={p} />)}
    </div>
  )
}

function ToolCallView({ tc }: { tc: OpenAiChatToolCall }) {
  const [open, setOpen] = useState(true)
  return (
    <div className="rounded bg-amber-50/60 border border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40 p-2 text-[11px]">
      <div className="flex items-center gap-2">
        <span className="font-medium">🔧 {tc.function.name}</span>
        <span className="font-mono text-[10px] text-muted-foreground">{tc.id}</span>
      </div>
      <details className="mt-1" open={open} onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}>
        <summary className="cursor-pointer text-muted-foreground text-[10px]">arguments</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
          {formatJson(safeParseJson(tc.function.arguments) ?? tc.function.arguments)}
        </pre>
      </details>
    </div>
  )
}

function safeParseJson(s: string): unknown {
  try { return JSON.parse(s) } catch { return null }
}

// ── messages ────────────────────────────────────────────────────────────────

const ROLE_STYLES: Record<string, string> = {
  system: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  developer: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  user: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  assistant: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
  tool: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
}

function messagePreview(msg: OpenAiChatMessage): string {
  if (msg.tool_calls && msg.tool_calls.length > 0) {
    return `🔧 ${msg.tool_calls.map((t) => t.function.name).slice(0, 2).join(", ")}`
  }
  if (typeof msg.content === "string") return msg.content.slice(0, 120)
  if (Array.isArray(msg.content)) {
    const firstText = msg.content.find((p) => p.type === "text")
    if (firstText && firstText.type === "text") return firstText.text.slice(0, 120)
    return msg.content.map((p) => p.type).join(", ")
  }
  return ""
}

function MessageRow({ msg, index, overlay }: { msg: OpenAiChatMessage; index: number; overlay?: CallOverlay | null }) {
  const [open, setOpen] = useState(false)
  const preview = messagePreview(msg)
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        <span className="w-5 shrink-0 text-[10px] tabular-nums text-muted-foreground">#{index + 1}</span>
        <span className={cn("shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium", ROLE_STYLES[msg.role])}>
          {msg.role}
        </span>
        <span className="flex-1 truncate text-muted-foreground">{preview}</span>
        {open ? <ChevronDown className="size-3 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3 shrink-0 text-muted-foreground" />}
      </button>
      {open && (
        <div className="space-y-2 border-t border-border/30 bg-muted/10 px-3 py-2">
          {msg.tool_call_id && (
            <div className="text-[10px] text-muted-foreground">
              tool_call_id: <span className="font-mono">{msg.tool_call_id}</span>
            </div>
          )}
          <MessageContent content={msg.content} isUserText={msg.role === "user"} overlay={overlay} />
          {msg.reasoning_content && (
            <div className="rounded bg-purple-50/60 border border-purple-200 dark:bg-purple-900/10 dark:border-purple-900/40 p-2 text-[11px]">
              <div className="font-medium text-purple-800 dark:text-purple-300">💭 reasoning_content</div>
              <pre className="mt-1 max-h-[400px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">
                {msg.reasoning_content}
              </pre>
            </div>
          )}
          {msg.tool_calls && msg.tool_calls.map((tc, i) => <ToolCallView key={i} tc={tc} />)}
          {msg.refusal && (
            <div className="rounded border border-red-300 bg-red-50 p-2 text-[11px] text-red-700 dark:bg-red-900/20 dark:text-red-300">
              🚫 refusal: {msg.refusal}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function MessagesSection({ request, overlay }: { request: OpenAiChatRequest; overlay?: CallOverlay | null }) {
  const [open, setOpen] = useState(true)
  if (request.messages.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Messages</span>
        <span className="text-muted-foreground">({request.messages.length})</span>
      </button>
      {open && <div>{request.messages.map((m, i) => <MessageRow key={i} msg={m} index={i} overlay={overlay} />)}</div>}
    </div>
  )
}

// ── tools / response_format / sampling ─────────────────────────────────────

function ToolsSection({ request }: { request: OpenAiChatRequest }) {
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
              <div className="font-medium flex items-center gap-2">
                {t.function.name}
                {t.function.strict && (
                  <span className="rounded bg-emerald-100 px-1 py-0.5 text-[9px] text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300">
                    strict
                  </span>
                )}
              </div>
              {t.function.description && <div className="text-muted-foreground">{t.function.description}</div>}
              <details className="mt-1">
                <summary className="cursor-pointer text-[10px] text-muted-foreground">parameters</summary>
                <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
                  {formatJson(t.function.parameters)}
                </pre>
              </details>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function ResponseFormatSection({ rf }: { rf: OpenAiChatResponseFormat }) {
  if (!rf) return null
  if (rf.kind === "text" || rf.kind === "json_object") {
    return (
      <div className="rounded border border-border/60 bg-background px-3 py-2 text-[11px]">
        <span className="font-medium">Response format</span>{" "}
        <span className="inline-flex items-center gap-1 rounded bg-muted px-1.5 py-0.5 text-[10px]">
          <FileJson className="size-2.5" />
          {rf.kind}
        </span>
      </div>
    )
  }
  if (rf.kind === "json_schema") {
    return (
      <div className="rounded border border-border/60 bg-background text-[11px]">
        <div className="flex items-center gap-2 border-b border-border/40 px-3 py-2">
          <FileJson className="size-3" />
          <span className="font-medium">JSON schema:</span>
          <span className="font-mono">{rf.name}</span>
          {rf.strict && (
            <span className="rounded bg-emerald-100 px-1 py-0.5 text-[9px] text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300">
              strict
            </span>
          )}
        </div>
        {rf.description && <div className="border-b border-border/40 px-3 py-1 text-muted-foreground">{rf.description}</div>}
        <details>
          <summary className="cursor-pointer px-3 py-2 text-muted-foreground">schema</summary>
          <pre className="mt-1 max-h-[400px] overflow-auto whitespace-pre-wrap bg-muted/20 p-3 font-mono text-[10px]">
            {formatJson(rf.schema)}
          </pre>
        </details>
      </div>
    )
  }
  return (
    <details className="rounded border border-border/60 bg-background px-3 py-2 text-[11px]">
      <summary className="cursor-pointer text-red-600">⚠️ unknown response_format</summary>
      <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
        {formatJson(rf.raw)}
      </pre>
    </details>
  )
}

function SamplingSection({ request }: { request: OpenAiChatRequest }) {
  const [open, setOpen] = useState(false)
  const s = request.sampling
  const rows: Array<[string, string | number | null]> = [
    ["model", request.model],
    ["max_completion_tokens", s.max_completion_tokens ?? s.max_tokens],
    ["temperature", s.temperature],
    ["top_p", s.top_p],
    ["n", s.n],
    ["seed", s.seed],
    ["stream", s.stream == null ? null : s.stream ? "true" : "false"],
    ["stream_include_usage", s.stream_include_usage == null ? null : s.stream_include_usage ? "true" : "false"],
    ["stop", s.stop.length ? s.stop.join(", ") : null],
    ["tool_choice", s.tool_choice !== undefined ? (typeof s.tool_choice === "string" ? s.tool_choice : formatJson(s.tool_choice)) : null],
    ["parallel_tool_calls", s.parallel_tool_calls == null ? null : s.parallel_tool_calls ? "true" : "false"],
    ["frequency_penalty", s.frequency_penalty],
    ["presence_penalty", s.presence_penalty],
    ["logprobs", s.logprobs == null ? null : s.logprobs ? "true" : "false"],
    ["top_logprobs", s.top_logprobs],
    ["service_tier", s.service_tier],
    ["user", s.user],
    ["store", s.store == null ? null : s.store ? "true" : "false"],
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
          {s.logit_bias && (
            <div className="contents">
              <span className="text-muted-foreground">logit_bias</span>
              <pre className="max-h-[100px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(s.logit_bias)}</pre>
            </div>
          )}
          {s.metadata && (
            <div className="contents">
              <span className="text-muted-foreground">metadata</span>
              <pre className="max-h-[100px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(s.metadata)}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}

// ── response side ──────────────────────────────────────────────────────────

function FinishReasonBadge({ reason }: { reason: string | null }) {
  if (!reason) return null
  const styles: Record<string, string> = {
    stop: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
    length: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    tool_calls: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
    function_call: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
    content_filter: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
  }
  return (
    <span className={cn("rounded px-1.5 py-0.5 text-[10px] font-medium", styles[reason] ?? "bg-muted text-muted-foreground")}>
      {reason}
    </span>
  )
}

function LogprobsPanel({ choice }: { choice: OpenAiChatChoice }) {
  const [open, setOpen] = useState(false)
  if (!choice.logprobs || choice.logprobs.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-[11px]">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <Activity className="size-3 text-purple-700 dark:text-purple-400" />
        <span className="font-medium">Logprobs</span>
        <span className="text-muted-foreground">({choice.logprobs.length} tokens)</span>
      </button>
      {open && (
        <div className="max-h-[400px] overflow-auto">
          <table className="w-full text-[10px]">
            <thead className="sticky top-0 bg-background">
              <tr className="border-b border-border/60">
                <th className="px-2 py-1 text-left">token</th>
                <th className="px-2 py-1 text-right">logprob</th>
                <th className="px-2 py-1 text-left">top alternatives</th>
              </tr>
            </thead>
            <tbody>
              {choice.logprobs.map((l, i) => (
                <tr key={i} className="border-b border-border/20">
                  <td className="px-2 py-1 font-mono">{JSON.stringify(l.token)}</td>
                  <td className="px-2 py-1 text-right font-mono tabular-nums">{l.logprob.toFixed(3)}</td>
                  <td className="px-2 py-1 font-mono text-muted-foreground">
                    {l.top_logprobs
                      .map((t) => `${JSON.stringify(t.token)}@${t.logprob.toFixed(2)}`)
                      .join(", ")}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}

function ResponseCard({ response }: { response: OpenAiChatResponse }) {
  if (response.choices.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No choices in response.</div>
  }
  return (
    <div className="space-y-2">
      {response.choices.map((c, i) => (
        <ChoiceCard key={i} choice={c} />
      ))}
      <UsageCard response={response} />
    </div>
  )
}

function ChoiceCard({ choice }: { choice: OpenAiChatChoice }) {
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2 text-[10px] text-muted-foreground">
        <span>choice #{choice.index}</span>
        <FinishReasonBadge reason={typeof choice.finish_reason === "string" ? choice.finish_reason : null} />
      </div>
      <div className="space-y-2">
        <MessageContent content={choice.message.content} isUserText={false} />
        {choice.message.reasoning_content && (
          <div className="rounded bg-purple-50/60 border border-purple-200 dark:bg-purple-900/10 dark:border-purple-900/40 p-2 text-[11px]">
            <div className="font-medium text-purple-800 dark:text-purple-300">💭 reasoning_content</div>
            <pre className="mt-1 max-h-[400px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">
              {choice.message.reasoning_content}
            </pre>
          </div>
        )}
        {choice.message.tool_calls?.map((tc, i) => <ToolCallView key={i} tc={tc} />)}
        {choice.message.refusal && (
          <div className="rounded border border-red-300 bg-red-50 p-2 text-[11px] text-red-700 dark:bg-red-900/20 dark:text-red-300">
            🚫 refusal: {choice.message.refusal}
          </div>
        )}
      </div>
      <LogprobsPanel choice={choice} />
    </div>
  )
}

function UsageCard({ response }: { response: OpenAiChatResponse }) {
  const u = response.usage
  return (
    <div className="rounded border border-border/60 bg-background p-3 text-[11px]">
      <div className="mb-1 font-medium">Usage</div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5">
        <span className="text-muted-foreground">prompt</span>
        <span className="tabular-nums">{u.prompt_tokens ?? "—"}</span>
        <span className="text-muted-foreground">completion</span>
        <span className="tabular-nums">{u.completion_tokens ?? "—"}</span>
        <span className="text-muted-foreground">total</span>
        <span className="tabular-nums">{u.total_tokens ?? "—"}</span>
        {u.cached_prompt_tokens != null && (
          <>
            <span className="text-muted-foreground">cached</span>
            <span className="tabular-nums text-purple-700 dark:text-purple-300">{u.cached_prompt_tokens}</span>
          </>
        )}
        {u.reasoning_tokens != null && (
          <>
            <span className="text-muted-foreground">reasoning</span>
            <span className="tabular-nums text-purple-700 dark:text-purple-300">{u.reasoning_tokens}</span>
          </>
        )}
        {response.system_fingerprint && (
          <>
            <span className="text-muted-foreground">fingerprint</span>
            <span className="font-mono text-[10px]">{response.system_fingerprint}</span>
          </>
        )}
        {response.service_tier && (
          <>
            <span className="text-muted-foreground">service_tier</span>
            <span className="font-mono text-[10px]">{response.service_tier}</span>
          </>
        )}
      </div>
    </div>
  )
}

// ── exported views ─────────────────────────────────────────────────────────

export interface OpenAiChatCallViewProps {
  requestBody: string | null
  responseBody: string | null
  overlay?: CallOverlay | null
  hasRequestBody: boolean
}

export function OpenAiChatCallView({
  requestBody,
  responseBody,
  overlay,
  hasRequestBody,
}: OpenAiChatCallViewProps) {
  const call = useMemo(() => parseOpenAiChatCall(requestBody, responseBody), [requestBody, responseBody])
  return (
    <>
      <section className="border-l-2 border-muted-foreground/30 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">Input</div>
        {!hasRequestBody ? (
          <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
            Request body not captured.
          </div>
        ) : (
          <div className="space-y-2">
            <MessagesSection request={call.request} overlay={overlay} />
            <ToolsSection request={call.request} />
            <ResponseFormatSection rf={call.request.response_format} />
            <SamplingSection request={call.request} />
          </div>
        )}
      </section>
      <section className="border-l-2 border-emerald-500/40 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
          Output
        </div>
        <ResponseCard response={call.response} />
      </section>
    </>
  )
}

export function OpenAiChatOutputBlocks({ response }: { response: OpenAiChatResponse }) {
  if (response.choices.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response content.</div>
  }
  return (
    <div className="space-y-2">
      {response.choices.map((c, i) => <ChoiceCard key={i} choice={c} />)}
    </div>
  )
}

export function openaiChatParseForOutput(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
) {
  const call = parseOpenAiChatCall(requestBody, responseBody)
  return { response: call.response }
}
