import { useMemo, useState } from "react"
import { ChevronRight, ChevronDown } from "lucide-react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { parseGeminiAiStudioCall } from "@/lib/wire-apis/gemini-aistudio"
import type {
  GeminiContent,
  GeminiPart,
  GeminiRequest,
  GeminiResponse,
} from "@/lib/wire-apis/gemini-aistudio/types"

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

// ── part sub-renderers ─────────────────────────────────────────────────────

function TextPartView({ text }: { text: string }) {
  return (
    <div className="text-[11px]">
      <Markdown text={text} />
    </div>
  )
}

function ThoughtPartView({ text }: { text: string }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded bg-purple-50/60 border border-purple-200 dark:bg-purple-900/10 dark:border-purple-900/40 p-2 text-[11px]">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 text-left"
      >
        {open
          ? <ChevronDown className="size-3 text-purple-700 dark:text-purple-400" />
          : <ChevronRight className="size-3 text-purple-700 dark:text-purple-400" />}
        <span className="font-medium text-purple-800 dark:text-purple-300">💭 thinking</span>
        <span className="text-[10px] text-muted-foreground">{formatSize(byteLength(text))}</span>
      </button>
      {open && (
        <pre className="mt-2 max-h-[400px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">
          {text}
        </pre>
      )}
    </div>
  )
}

function FunctionCallPartView({ name, args }: { name: string; args: unknown }) {
  const [argsOpen, setArgsOpen] = useState(true)
  return (
    <div className="rounded bg-amber-50/60 border border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40 p-2 text-[11px]">
      <div className="font-mono text-[9px] uppercase tracking-wider text-amber-700/80 dark:text-amber-400/80">
        function_call
      </div>
      <div className="mt-1">
        <span className="text-muted-foreground">name:</span>{" "}
        <span className="font-medium">{name}</span>
      </div>
      <details
        className="mt-1"
        open={argsOpen}
        onToggle={(e) => setArgsOpen((e.target as HTMLDetailsElement).open)}
      >
        <summary className="cursor-pointer text-muted-foreground text-[10px]">args</summary>
        <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
          {formatJson(args)}
        </pre>
      </details>
    </div>
  )
}

function FunctionResponsePartView({ name, response }: { name: string; response: unknown }) {
  const responseStr = typeof response === "string" ? response : formatJson(response)
  return (
    <div className="rounded bg-muted/40 p-2 text-[11px]">
      <div className="font-mono text-[9px] uppercase tracking-wider text-muted-foreground">
        function_response
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5">
        <span>
          <span className="text-muted-foreground">name:</span>{" "}
          <span className="font-medium">{name}</span>
        </span>
        <span className="text-muted-foreground">{formatSize(byteLength(responseStr))}</span>
      </div>
      <pre className="mt-1.5 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
        {responseStr}
      </pre>
    </div>
  )
}

function InlineDataPartView({ mimeType, data }: { mimeType: string; data: string }) {
  if (mimeType.startsWith("image/")) {
    const src = `data:${mimeType};base64,${data}`
    return (
      <div className="flex items-start gap-2 text-[11px]">
        <span className="text-muted-foreground">🖼️ image ({mimeType})</span>
        <img src={src} alt="" className="max-h-40 max-w-xs rounded border border-border" />
      </div>
    )
  }
  return (
    <div className="text-[11px] text-muted-foreground">
      📎 inline_data ({mimeType}, {formatSize(data.length)})
    </div>
  )
}

function PartView({ part }: { part: GeminiPart }) {
  switch (part.type) {
    case "text":
      return <TextPartView text={part.text} />
    case "thought":
      return <ThoughtPartView text={part.text} />
    case "function_call":
      return <FunctionCallPartView name={part.name} args={part.args} />
    case "function_response":
      return <FunctionResponsePartView name={part.name} response={part.response} />
    case "inline_data":
      return <InlineDataPartView mimeType={part.mimeType} data={part.data} />
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

// ── content list (for request) ─────────────────────────────────────────────

const ROLE_STYLES: Record<"user" | "model", string> = {
  user: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  model: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
}

function contentPreview(c: GeminiContent): string {
  for (const p of c.parts) {
    if (p.type === "text") return p.text.slice(0, 120)
    if (p.type === "thought") return `💭 ${p.text.slice(0, 120)}`
    if (p.type === "function_call") return `🔧 ${p.name}(${formatJson(p.args).slice(0, 60)})`
    if (p.type === "function_response") return `⤷ ${p.name}`
  }
  return ""
}

function partTypeCounts(c: GeminiContent): Partial<Record<GeminiPart["type"], number>> {
  const counts: Partial<Record<GeminiPart["type"], number>> = {}
  for (const p of c.parts) counts[p.type] = (counts[p.type] ?? 0) + 1
  return counts
}

const CHIP_TYPE_ORDER: ReadonlyArray<GeminiPart["type"]> = [
  "thought",
  "function_call",
  "function_response",
  "inline_data",
  "unknown",
]

function PartChips({ counts }: { counts: Partial<Record<GeminiPart["type"], number>> }) {
  const chips: Array<{ type: GeminiPart["type"]; count: number }> = []
  for (const t of CHIP_TYPE_ORDER) {
    const n = counts[t] ?? 0
    if (n > 0) chips.push({ type: t, count: n })
  }
  if (chips.length === 0) return null
  return (
    <span className="flex shrink-0 items-center gap-1">
      {chips.map(({ type, count }) => {
        const label = count > 1 ? `${type} ×${count}` : type
        const isFnResp = type === "function_response"
        return (
          <span
            key={type}
            className={cn(
              "rounded px-1 py-0.5 text-[9px]",
              isFnResp
                ? "border border-amber-300/70 bg-amber-50 text-amber-800 dark:border-amber-900/50 dark:bg-amber-900/20 dark:text-amber-300"
                : "border border-border/60 bg-muted/40 text-muted-foreground",
            )}
          >
            {label}
          </span>
        )
      })}
    </span>
  )
}

function ContentRow({ content, index }: { content: GeminiContent; index: number }) {
  const [open, setOpen] = useState(false)
  const preview = contentPreview(content)
  const counts = partTypeCounts(content)
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        <span className="w-5 shrink-0 text-[10px] tabular-nums text-muted-foreground">#{index + 1}</span>
        <span
          className={cn(
            "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium",
            ROLE_STYLES[content.role],
          )}
        >
          {content.role}
        </span>
        <PartChips counts={counts} />
        <span className="flex-1 truncate text-muted-foreground">{preview}</span>
        {open
          ? <ChevronDown className="size-3 shrink-0 text-muted-foreground" />
          : <ChevronRight className="size-3 shrink-0 text-muted-foreground" />}
      </button>
      {open && (
        <div className="space-y-2 border-t border-border/30 bg-muted/10 px-3 py-2">
          {content.parts.map((p, i) => <PartView key={i} part={p} />)}
        </div>
      )}
    </div>
  )
}

function ContentsSection({ contents }: { contents: GeminiContent[] }) {
  const [open, setOpen] = useState(false)
  if (contents.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open
          ? <ChevronDown className="size-3 text-muted-foreground" />
          : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Contents</span>
        <span className="text-muted-foreground">({contents.length})</span>
      </button>
      {open && <div>{contents.map((c, i) => <ContentRow key={i} content={c} index={i} />)}</div>}
    </div>
  )
}

// ── system / tools / config sections ───────────────────────────────────────

function SystemInstructionSection({ content }: { content: GeminiContent | null }) {
  const [open, setOpen] = useState(false)
  if (!content || content.parts.length === 0) return null
  const text = content.parts
    .filter((p): p is Extract<GeminiPart, { type: "text" }> => p.type === "text")
    .map((p) => p.text)
    .join("\n\n")
  if (!text) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open
          ? <ChevronDown className="size-3 text-muted-foreground" />
          : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">System Instruction</span>
        <span className="text-muted-foreground">· {formatSize(byteLength(text))}</span>
      </button>
      {open && <div className="px-3 py-2 text-[11px]"><Markdown text={text} /></div>}
    </div>
  )
}

function ToolsSection({ request }: { request: GeminiRequest }) {
  const [open, setOpen] = useState(false)
  if (request.tools.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open
          ? <ChevronDown className="size-3 text-muted-foreground" />
          : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Tools</span>
        <span className="text-muted-foreground">({request.tools.length})</span>
      </button>
      {open && (
        <div className="space-y-2 px-3 py-2">
          {request.tools.map((t, i) => (
            <div key={i} className="rounded border border-border/40 bg-muted/20 p-2 text-[11px]">
              <div className="font-medium">{t.name}</div>
              {t.description && <div className="text-muted-foreground">{t.description}</div>}
              <details className="mt-1">
                <summary className="cursor-pointer text-[10px] text-muted-foreground">parameters</summary>
                <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
                  {formatJson(t.parametersJsonSchema)}
                </pre>
              </details>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function GenerationConfigSection({ request }: { request: GeminiRequest }) {
  const [open, setOpen] = useState(false)
  const g = request.generationConfig
  const rows: Array<[string, string | number | null]> = [
    ["model", request.model],
  ]
  if (g) {
    rows.push(
      ["temperature", g.temperature],
      ["topP", g.topP],
      ["topK", g.topK],
      ["candidateCount", g.candidateCount],
      ["maxOutputTokens", g.maxOutputTokens],
    )
    if (g.thinkingConfig) {
      rows.push(
        ["thinkingLevel", g.thinkingConfig.thinkingLevel],
        ["thinkingBudget", g.thinkingConfig.thinkingBudget],
        [
          "includeThoughts",
          g.thinkingConfig.includeThoughts == null
            ? null
            : g.thinkingConfig.includeThoughts ? "true" : "false",
        ],
      )
    }
  }
  const visible = rows.filter(([, v]) => v != null && v !== "")
  if (visible.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {open
          ? <ChevronDown className="size-3 text-muted-foreground" />
          : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Parameters</span>
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

function FinishReasonBadge({ reason }: { reason: string | null }) {
  if (!reason) return null
  const styles: Record<string, string> = {
    STOP: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
    TOOL_USE: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
    MAX_TOKENS: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    SAFETY: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    RECITATION: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    PROHIBITED_CONTENT: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    MALFORMED_FUNCTION_CALL: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
  }
  return (
    <span
      className={cn(
        "rounded px-1.5 py-0.5 text-[10px] font-medium",
        styles[reason] ?? "bg-muted text-muted-foreground",
      )}
    >
      {reason}
    </span>
  )
}

function UsageCard({ response }: { response: GeminiResponse }) {
  const u = response.usageMetadata
  const firstFinish = response.candidates[0]?.finishReason ?? null
  return (
    <div className="rounded border border-border/60 bg-background p-3 text-[11px]">
      <div className="mb-1 flex items-center gap-2">
        <span className="font-medium">Usage</span>
        <FinishReasonBadge reason={typeof firstFinish === "string" ? firstFinish : null} />
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5">
        <span className="text-muted-foreground">prompt</span>
        <span className="tabular-nums">{u.promptTokenCount ?? "—"}</span>
        <span className="text-muted-foreground">candidates</span>
        <span className="tabular-nums">{u.candidatesTokenCount ?? "—"}</span>
        <span className="text-muted-foreground">total</span>
        <span className="tabular-nums">{u.totalTokenCount ?? "—"}</span>
        {u.cachedContentTokenCount != null && (
          <>
            <span className="text-muted-foreground">cached</span>
            <span className="tabular-nums text-purple-700 dark:text-purple-300">
              {u.cachedContentTokenCount}
            </span>
          </>
        )}
        {u.thoughtsTokenCount != null && (
          <>
            <span className="text-muted-foreground" title="Subset of candidates — already counted there">
              thoughts*
            </span>
            <span className="tabular-nums text-muted-foreground">{u.thoughtsTokenCount}</span>
          </>
        )}
      </div>
    </div>
  )
}

function GeminiOutputBlocksInternal({ response }: { response: GeminiResponse }) {
  if (response.candidates.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response candidates.</div>
  }
  // Single-candidate case (the overwhelming majority): inline parts directly.
  // Multi-candidate (rare; when generationConfig.candidateCount > 1) is shown
  // with simple separators so users see all of them.
  if (response.candidates.length === 1) {
    return (
      <div className="space-y-2">
        {response.candidates[0].content.parts.map((p, i) => <PartView key={i} part={p} />)}
      </div>
    )
  }
  return (
    <div className="space-y-3">
      {response.candidates.map((c, i) => (
        <div key={i}>
          <div className="mb-1 text-[10px] font-semibold text-muted-foreground">
            Candidate #{(c.index ?? i) + 1}
            {c.finishReason && (
              <span className="ml-2"><FinishReasonBadge reason={typeof c.finishReason === "string" ? c.finishReason : null} /></span>
            )}
          </div>
          <div className="space-y-2">
            {c.content.parts.map((p, j) => <PartView key={j} part={p} />)}
          </div>
        </div>
      ))}
    </div>
  )
}

// ── exported views ─────────────────────────────────────────────────────────

export interface GeminiAiStudioCallViewProps {
  requestBody: string | null
  responseBody: string | null
  hasRequestBody: boolean
}

export function GeminiAiStudioCallView({
  requestBody,
  responseBody,
  hasRequestBody,
}: GeminiAiStudioCallViewProps) {
  const call = useMemo(
    () => parseGeminiAiStudioCall(requestBody, responseBody),
    [requestBody, responseBody],
  )

  return (
    <>
      <section className="border-l-2 border-muted-foreground/30 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
          Input
        </div>
        {!hasRequestBody ? (
          <div className="rounded border border-border/60 bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
            Request body not captured.
          </div>
        ) : (
          <div className="space-y-2">
            <SystemInstructionSection content={call.request.systemInstruction} />
            <ContentsSection contents={call.request.contents} />
            <ToolsSection request={call.request} />
            <GenerationConfigSection request={call.request} />
          </div>
        )}
      </section>
      <section className="border-l-2 border-emerald-500/40 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">
          Output
        </div>
        <GeminiOutputBlocksInternal response={call.response} />
        <div className="mt-2">
          <UsageCard response={call.response} />
        </div>
      </section>
    </>
  )
}

export function GeminiAiStudioOutputBlocks({ response }: { response: GeminiResponse }) {
  return <GeminiOutputBlocksInternal response={response} />
}

// eslint-disable-next-line react-refresh/only-export-components
export function geminiAiStudioParseForOutput(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
) {
  const call = parseGeminiAiStudioCall(requestBody, responseBody)
  return { response: call.response }
}

// ── Input subsection (functionResponse back-pointers + extra user text) ────

export interface GeminiAiStudioParsedInput {
  functionResponses: Array<{ name: string; response: string }>
  extraUserText: string | null
}

// eslint-disable-next-line react-refresh/only-export-components
export function geminiAiStudioParseForInput(
  requestBody: string | null | undefined,
): GeminiAiStudioParsedInput {
  if (!requestBody) return { functionResponses: [], extraUserText: null }
  const call = parseGeminiAiStudioCall(requestBody, null)
  // Take only the last user-role content — that's the delta from the prior call.
  const lastUser = [...call.request.contents].reverse().find((c) => c.role === "user")
  if (!lastUser) return { functionResponses: [], extraUserText: null }
  const functionResponses: GeminiAiStudioParsedInput["functionResponses"] = []
  let extraUserText: string | null = null
  for (const part of lastUser.parts) {
    if (part.type === "function_response") {
      const resp = typeof part.response === "string" ? part.response : formatJson(part.response)
      functionResponses.push({ name: part.name, response: resp })
    } else if (part.type === "text") {
      extraUserText = (extraUserText ?? "") + (extraUserText ? "\n\n" : "") + part.text
    }
  }
  return { functionResponses, extraUserText }
}

export function GeminiAiStudioInputBlocks({ parsed }: { parsed: GeminiAiStudioParsedInput }) {
  if (parsed.functionResponses.length === 0 && !parsed.extraUserText) {
    return <div className="text-[11px] text-muted-foreground italic">No input deltas.</div>
  }
  return (
    <div className="space-y-2">
      {parsed.functionResponses.map((fr, i) => (
        <div
          key={i}
          className="rounded border border-border/60 bg-muted/40 p-2 text-[11px]"
        >
          <div className="flex items-center gap-2">
            <span className="font-medium">⤷ function_response</span>
            <span className="font-mono text-[10px] text-muted-foreground">{fr.name}</span>
            <span className="text-[10px] text-muted-foreground">· {formatSize(byteLength(fr.response))}</span>
          </div>
          <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
            {fr.response}
          </pre>
        </div>
      ))}
      {parsed.extraUserText && (
        <div className="rounded border border-blue-200 bg-blue-50/60 p-3 text-[11px] dark:border-blue-900/40 dark:bg-blue-900/10">
          <Markdown text={parsed.extraUserText} />
        </div>
      )}
    </div>
  )
}
