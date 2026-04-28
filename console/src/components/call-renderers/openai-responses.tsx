import { useMemo, useState } from "react"
import { ChevronRight, ChevronDown, Link2 } from "lucide-react"
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { parseOpenAiResponsesCall } from "@/lib/wire-apis/openai-responses"
import type {
  ResponsesContentPart,
  ResponsesItem,
  ResponsesMessageItem,
  ResponsesReasoningItem,
  ResponsesRequest,
  ResponsesResponse,
  ResponsesToolDef,
} from "@/lib/wire-apis/openai-responses/types"
import type { CallOverlay } from "./overlays/types"
import { ToolUsePointer, ToolResultBackLink } from "@/components/turn-detail/tool-pointer"
import { classifyToolUseState, classifyToolResultState, type ToolIndex } from "@/lib/turn-index"

interface OutputCtx {
  toolIndex: ToolIndex
  callId: string
}
interface InputCtx {
  toolIndex: ToolIndex
}

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

function safeParseJson(s: string): unknown {
  try {
    return JSON.parse(s)
  } catch {
    return null
  }
}

// ── content parts ──────────────────────────────────────────────────────────

function ContentPartView({ part, renderUserText }: { part: ResponsesContentPart; renderUserText?: (t: string) => React.ReactNode }) {
  switch (part.type) {
    case "input_text":
    case "text":
    case "output_text": {
      const inner = renderUserText && part.type === "input_text"
        ? renderUserText(part.text)
        : <Markdown text={part.text} />
      return (
        <div className="text-[11px]">
          {inner}
          {"annotations" in part && part.annotations && part.annotations.length > 0 && (
            <details className="mt-1">
              <summary className="cursor-pointer text-[10px] text-muted-foreground">annotations ({part.annotations.length})</summary>
              <pre className="mt-1 max-h-[200px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
                {formatJson(part.annotations)}
              </pre>
            </details>
          )}
        </div>
      )
    }
    case "input_image":
      return (
        <div className="flex items-start gap-2 text-[11px]">
          <span className="text-muted-foreground">🖼️ image</span>
          {part.image_url && (
            <a href={part.image_url} className="font-mono hover:underline" target="_blank" rel="noreferrer">
              {part.image_url}
            </a>
          )}
          {part.file_id && <span className="font-mono text-muted-foreground">file: {part.file_id}</span>}
          {part.detail && <span className="text-muted-foreground">({part.detail})</span>}
        </div>
      )
    case "input_file":
      return (
        <div className="text-[11px] text-muted-foreground">
          📄 file {part.filename ? `"${part.filename}"` : ""}
          {part.file_id && <span className="ml-1 font-mono">id:{part.file_id}</span>}
          {part.file_data && <span className="ml-1">· {formatSize(byteLength(part.file_data))}</span>}
        </div>
      )
    case "refusal":
      return (
        <div className="rounded border border-red-300 bg-red-50 p-2 text-[11px] text-red-700 dark:bg-red-900/20 dark:text-red-300">
          🚫 refusal: {part.refusal}
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

// ── item sub-renderers ─────────────────────────────────────────────────────

const ROLE_STYLES: Record<string, string> = {
  system: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  developer: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  user: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  assistant: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
}

const ITEM_TYPE_STYLES: Record<string, string> = {
  function_call: "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
  function_call_output:
    "border border-amber-300/70 bg-amber-50 text-amber-800 dark:border-amber-900/50 dark:bg-amber-900/20 dark:text-amber-300",
  reasoning: "bg-purple-100 text-purple-800 dark:bg-purple-900/40 dark:text-purple-300",
  file_search_call: "bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300",
  web_search_call: "bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300",
  computer_call: "bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300",
  mcp_call: "bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300",
  unknown: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
}

const ITEM_KIND_ORDER: ResponsesItem["kind"][] = [
  "message",
  "reasoning",
  "function_call",
  "function_call_output",
  "file_search_call",
  "web_search_call",
  "computer_call",
  "mcp_call",
  "unknown",
]

function Chip({
  children,
  variant = "muted",
  title,
}: {
  children: React.ReactNode
  variant?: "muted" | "amber"
  title?: string
}) {
  return (
    <span
      title={title}
      className={cn(
        "rounded px-1 py-0.5 text-[9px]",
        variant === "amber"
          ? "border border-amber-300/70 bg-amber-50 text-amber-800 dark:border-amber-900/50 dark:bg-amber-900/20 dark:text-amber-300"
          : "border border-border/60 bg-muted/40 text-muted-foreground",
      )}
    >
      {children}
    </span>
  )
}

function InputItemRow({
  index,
  badgeLabel,
  badgeStyle,
  badgeTitle,
  chips,
  preview,
  children,
}: {
  index: number
  badgeLabel: string
  badgeStyle: string
  badgeTitle?: string
  chips?: React.ReactNode
  preview?: React.ReactNode
  children?: React.ReactNode
}) {
  const [open, setOpen] = useState(false)
  return (
    <div className="border-t border-border/40 first:border-t-0">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/40"
      >
        <span className="w-5 shrink-0 text-[10px] tabular-nums text-muted-foreground">#{index + 1}</span>
        <span
          title={badgeTitle}
          className={cn("shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium", badgeStyle)}
        >
          {badgeLabel}
        </span>
        {chips}
        <span className="flex-1 truncate text-muted-foreground">{preview}</span>
        {open ? <ChevronDown className="size-3 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3 shrink-0 text-muted-foreground" />}
      </button>
      {open && children && (
        <div className="space-y-2 border-t border-border/30 bg-muted/10 px-3 py-2">
          {children}
        </div>
      )}
    </div>
  )
}

function ItemTypeChips({ items }: { items: ResponsesItem[] }) {
  const counts: Partial<Record<ResponsesItem["kind"], number>> = {}
  for (const it of items) counts[it.kind] = (counts[it.kind] ?? 0) + 1
  const entries: Array<{ kind: ResponsesItem["kind"]; count: number }> = []
  for (const k of ITEM_KIND_ORDER) {
    const n = counts[k] ?? 0
    if (n > 0) entries.push({ kind: k, count: n })
  }
  if (entries.length === 0) return null
  return (
    <span className="flex flex-wrap items-center gap-1">
      {entries.map(({ kind, count }) => (
        <Chip key={kind} variant={kind === "function_call_output" ? "amber" : "muted"}>
          {count > 1 ? `${kind} ×${count}` : kind}
        </Chip>
      ))}
    </span>
  )
}

function MessageItemView({
  item,
  index,
  overlay,
}: {
  item: ResponsesMessageItem
  index: number
  overlay?: CallOverlay | null
}) {
  const UserMsg = overlay?.UserMessageContent
  const preview = (() => {
    if (typeof item.content === "string") return item.content.slice(0, 120)
    const first = item.content.find((p) => p.type === "input_text" || p.type === "output_text" || p.type === "text")
    if (first && (first.type === "input_text" || first.type === "output_text" || first.type === "text")) {
      return first.text.slice(0, 120)
    }
    return item.content.map((p) => p.type).join(", ")
  })()
  const partCounts: Record<string, number> = {}
  if (typeof item.content !== "string") {
    for (const p of item.content) {
      if (p.type === "input_text" || p.type === "output_text" || p.type === "text") continue
      partCounts[p.type] = (partCounts[p.type] ?? 0) + 1
    }
  }
  const partEntries = Object.entries(partCounts)
  const chips = partEntries.length > 0 ? (
    <span className="flex shrink-0 items-center gap-1">
      {partEntries.map(([type, n]) => (
        <Chip key={type}>{n > 1 ? `${type} ×${n}` : type}</Chip>
      ))}
    </span>
  ) : undefined
  return (
    <InputItemRow
      index={index}
      badgeLabel={item.role}
      badgeStyle={ROLE_STYLES[item.role] ?? "bg-muted"}
      badgeTitle={`role: ${item.role}`}
      chips={chips}
      preview={preview}
    >
      {typeof item.content === "string" ? (
        <div className="text-[11px]">
          {item.role === "user" && UserMsg ? <UserMsg text={item.content} /> : <Markdown text={item.content} />}
        </div>
      ) : (
        item.content.map((p, i) => (
          <ContentPartView
            key={i}
            part={p}
            renderUserText={item.role === "user" && UserMsg ? (t) => <UserMsg text={t} /> : undefined}
          />
        ))
      )}
    </InputItemRow>
  )
}

function FunctionCallItemView({
  item,
  index,
  ctx,
}: {
  item: Extract<ResponsesItem, { kind: "function_call" }>
  index: number
  ctx?: OutputCtx
}) {
  const parsed = safeParseJson(item.arguments)
  const entry = ctx?.toolIndex.get(item.call_id) ?? { origin: null, resolution: null }
  const state = ctx ? classifyToolUseState(entry) : "healthy"
  const argsSnippet = formatJson(parsed ?? item.arguments).replace(/\s+/g, " ").slice(0, 60)
  const preview = `🔧 ${item.name}(${argsSnippet})`
  const chips = item.status && item.status !== "completed" ? (
    <span className="flex shrink-0 items-center gap-1">
      <Chip>{`status:${item.status}`}</Chip>
    </span>
  ) : undefined
  return (
    <InputItemRow
      index={index}
      badgeLabel="function_call"
      badgeStyle={ITEM_TYPE_STYLES.function_call}
      badgeTitle="type: function_call"
      chips={chips}
      preview={preview}
    >
      <div className="font-mono text-[9px] uppercase tracking-wider text-amber-700/80 dark:text-amber-400/80">
        function_call
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
        <span>
          <span className="text-muted-foreground">name:</span>{" "}
          <span className="font-medium">{item.name}</span>
        </span>
        <span>
          <span className="text-muted-foreground">call_id:</span>{" "}
          <span className="font-mono text-[10px]">{item.call_id}</span>
        </span>
        {item.status && (
          <span>
            <span className="text-muted-foreground">status:</span>{" "}
            <span className="font-mono text-[10px]">{item.status}</span>
          </span>
        )}
      </div>
      <div className="mt-1.5">
        <div className="mb-1 text-[10px] text-muted-foreground">arguments</div>
        <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
          {formatJson(parsed ?? item.arguments)}
        </pre>
      </div>
      {ctx && (
        <div className="mt-1">
          <ToolUsePointer state={state} resolution={entry.resolution} />
        </div>
      )}
    </InputItemRow>
  )
}

function FunctionCallOutputItemView({
  item,
  index,
  renderToolResult,
}: {
  item: Extract<ResponsesItem, { kind: "function_call_output" }>
  index: number
  renderToolResult?: (content: string, isError: boolean) => React.ReactNode
}) {
  const str = typeof item.output === "string" ? item.output : formatJson(item.output)
  const rendered = renderToolResult ? renderToolResult(str, false) : (
    <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{str}</pre>
  )
  const preview = `⤷ ${item.call_id} · ${formatSize(byteLength(str))}`
  return (
    <InputItemRow
      index={index}
      badgeLabel="function_call_output"
      badgeStyle={ITEM_TYPE_STYLES.function_call_output}
      badgeTitle="type: function_call_output"
      preview={preview}
    >
      <div className="font-mono text-[9px] uppercase tracking-wider text-muted-foreground">
        function_call_output
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
        <span>
          <span className="text-muted-foreground">call_id:</span>{" "}
          <span className="font-mono text-[10px]">{item.call_id}</span>
        </span>
        <span className="text-muted-foreground">{formatSize(byteLength(str))}</span>
      </div>
      <div className="mt-1.5">
        <div className="mb-1 text-[10px] text-muted-foreground">output</div>
        {rendered}
      </div>
    </InputItemRow>
  )
}

function ReasoningItemView({ item, index }: { item: ResponsesReasoningItem; index: number }) {
  const body = item.summary.join("\n\n")
  const firstSummary = item.summary[0]?.slice(0, 120)
  const preview = firstSummary || (item.encrypted_content ? "(encrypted)" : "(no summary)")
  const chipNodes: React.ReactNode[] = []
  if (item.encrypted_content) chipNodes.push(<Chip key="enc">encrypted</Chip>)
  if (item.status && item.status !== "completed") chipNodes.push(<Chip key="status">{`status:${item.status}`}</Chip>)
  if (item.summary.length > 1) chipNodes.push(<Chip key="sum">{`summary ×${item.summary.length}`}</Chip>)
  const chips = chipNodes.length > 0 ? (
    <span className="flex shrink-0 items-center gap-1">{chipNodes}</span>
  ) : undefined
  return (
    <InputItemRow
      index={index}
      badgeLabel="reasoning"
      badgeStyle={ITEM_TYPE_STYLES.reasoning}
      badgeTitle="type: reasoning"
      chips={chips}
      preview={preview}
    >
      <div className="font-mono text-[9px] uppercase tracking-wider text-purple-700/80 dark:text-purple-400/80">
        reasoning
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
        {item.id && (
          <span>
            <span className="text-muted-foreground">id:</span>{" "}
            <span className="font-mono text-[10px]">{item.id}</span>
          </span>
        )}
        {item.status && (
          <span>
            <span className="text-muted-foreground">status:</span>{" "}
            <span className="font-mono text-[10px]">{item.status}</span>
          </span>
        )}
        {item.summary.length > 0 && (
          <span>
            <span className="text-muted-foreground">summary:</span>{" "}
            <span className="font-mono text-[10px]">{item.summary.length} items</span>
          </span>
        )}
        {item.encrypted_content && (
          <span className="text-muted-foreground">encrypted_content</span>
        )}
      </div>
      <div className="mt-1.5">
        {body ? (
          <pre className="max-h-[400px] overflow-auto whitespace-pre-wrap font-sans text-[11px]">{body}</pre>
        ) : (
          <div className="text-[10px] italic text-muted-foreground">(no summary exposed; may be encrypted)</div>
        )}
      </div>
    </InputItemRow>
  )
}

function FileSearchCallItemView({
  item,
  index,
}: {
  item: Extract<ResponsesItem, { kind: "file_search_call" }>
  index: number
}) {
  const queries = item.queries ?? []
  const results = item.results ?? []
  const preview = queries[0] ?? (results.length > 0 ? `${results.length} results` : "")
  const chipNodes: React.ReactNode[] = []
  if (item.status && item.status !== "completed") chipNodes.push(<Chip key="status">{`status:${item.status}`}</Chip>)
  if (queries.length > 0) chipNodes.push(<Chip key="q">{`queries ×${queries.length}`}</Chip>)
  if (results.length > 0) chipNodes.push(<Chip key="r">{`results ×${results.length}`}</Chip>)
  const chips = chipNodes.length > 0 ? (
    <span className="flex shrink-0 items-center gap-1">{chipNodes}</span>
  ) : undefined
  return (
    <InputItemRow
      index={index}
      badgeLabel="file_search_call"
      badgeStyle={ITEM_TYPE_STYLES.file_search_call}
      badgeTitle="type: file_search_call"
      chips={chips}
      preview={preview}
    >
      <div className="font-mono text-[9px] uppercase tracking-wider text-cyan-700/80 dark:text-cyan-400/80">
        file_search_call
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
        {item.id && (
          <span>
            <span className="text-muted-foreground">id:</span>{" "}
            <span className="font-mono text-[10px]">{item.id}</span>
          </span>
        )}
        {item.status && (
          <span>
            <span className="text-muted-foreground">status:</span>{" "}
            <span className="font-mono text-[10px]">{item.status}</span>
          </span>
        )}
      </div>
      {queries.length > 0 && (
        <div className="mt-1.5">
          <div className="mb-1 text-[10px] text-muted-foreground">queries</div>
          <ul className="ml-4 list-disc text-[11px]">
            {queries.map((q, i) => <li key={i} className="font-mono">{q}</li>)}
          </ul>
        </div>
      )}
      {results.length > 0 && (
        <div className="mt-1.5">
          <div className="mb-1 text-[10px] text-muted-foreground">results ({results.length})</div>
          <pre className="max-h-[300px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(results)}</pre>
        </div>
      )}
    </InputItemRow>
  )
}

function WebSearchCallItemView({
  item,
  index,
}: {
  item: Extract<ResponsesItem, { kind: "web_search_call" }>
  index: number
}) {
  const actionStr = item.action != null ? formatJson(item.action) : ""
  const preview = actionStr.replace(/\s+/g, " ").slice(0, 80)
  const chipNodes: React.ReactNode[] = []
  if (item.status && item.status !== "completed") chipNodes.push(<Chip key="status">{`status:${item.status}`}</Chip>)
  const chips = chipNodes.length > 0 ? (
    <span className="flex shrink-0 items-center gap-1">{chipNodes}</span>
  ) : undefined
  return (
    <InputItemRow
      index={index}
      badgeLabel="web_search_call"
      badgeStyle={ITEM_TYPE_STYLES.web_search_call}
      badgeTitle="type: web_search_call"
      chips={chips}
      preview={preview}
    >
      <div className="font-mono text-[9px] uppercase tracking-wider text-cyan-700/80 dark:text-cyan-400/80">
        web_search_call
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
        {item.id && (
          <span>
            <span className="text-muted-foreground">id:</span>{" "}
            <span className="font-mono text-[10px]">{item.id}</span>
          </span>
        )}
        {item.status && (
          <span>
            <span className="text-muted-foreground">status:</span>{" "}
            <span className="font-mono text-[10px]">{item.status}</span>
          </span>
        )}
      </div>
      {item.action != null && (
        <div className="mt-1.5">
          <div className="mb-1 text-[10px] text-muted-foreground">action</div>
          <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(item.action)}</pre>
        </div>
      )}
    </InputItemRow>
  )
}

function ComputerCallItemView({
  item,
  index,
}: {
  item: Extract<ResponsesItem, { kind: "computer_call" }>
  index: number
}) {
  const actionStr = item.action != null ? formatJson(item.action) : ""
  const preview = actionStr.replace(/\s+/g, " ").slice(0, 80)
  const chipNodes: React.ReactNode[] = []
  if (item.status && item.status !== "completed") chipNodes.push(<Chip key="status">{`status:${item.status}`}</Chip>)
  const chips = chipNodes.length > 0 ? (
    <span className="flex shrink-0 items-center gap-1">{chipNodes}</span>
  ) : undefined
  return (
    <InputItemRow
      index={index}
      badgeLabel="computer_call"
      badgeStyle={ITEM_TYPE_STYLES.computer_call}
      badgeTitle="type: computer_call"
      chips={chips}
      preview={preview}
    >
      <div className="font-mono text-[9px] uppercase tracking-wider text-cyan-700/80 dark:text-cyan-400/80">
        computer_call
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
        {item.status && (
          <span>
            <span className="text-muted-foreground">status:</span>{" "}
            <span className="font-mono text-[10px]">{item.status}</span>
          </span>
        )}
      </div>
      {item.action != null && (
        <div className="mt-1.5">
          <div className="mb-1 text-[10px] text-muted-foreground">action</div>
          <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(item.action)}</pre>
        </div>
      )}
    </InputItemRow>
  )
}

function McpCallItemView({
  item,
  index,
}: {
  item: Extract<ResponsesItem, { kind: "mcp_call" }>
  index: number
}) {
  const argsSnippet = item.arguments?.replace(/\s+/g, " ").slice(0, 60) ?? ""
  const preview = `${item.name}${argsSnippet ? `(${argsSnippet})` : ""}`
  const chipNodes: React.ReactNode[] = []
  if (item.server_label) chipNodes.push(<Chip key="server">{`server:${item.server_label}`}</Chip>)
  if (item.error) chipNodes.push(<Chip key="err">error</Chip>)
  const chips = chipNodes.length > 0 ? (
    <span className="flex shrink-0 items-center gap-1">{chipNodes}</span>
  ) : undefined
  return (
    <InputItemRow
      index={index}
      badgeLabel="mcp_call"
      badgeStyle={ITEM_TYPE_STYLES.mcp_call}
      badgeTitle="type: mcp_call"
      chips={chips}
      preview={preview}
    >
      <div className="font-mono text-[9px] uppercase tracking-wider text-cyan-700/80 dark:text-cyan-400/80">
        mcp_call
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px]">
        <span>
          <span className="text-muted-foreground">name:</span>{" "}
          <span className="font-medium">{item.name}</span>
        </span>
        {item.server_label && (
          <span>
            <span className="text-muted-foreground">server_label:</span>{" "}
            <span className="font-mono text-[10px]">{item.server_label}</span>
          </span>
        )}
      </div>
      {item.arguments && (
        <div className="mt-1.5">
          <div className="mb-1 text-[10px] text-muted-foreground">arguments</div>
          <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{item.arguments}</pre>
        </div>
      )}
      {item.error && (
        <div className="mt-1.5 rounded border border-red-300 bg-red-50 px-2 py-1 text-[11px] text-red-700 dark:border-red-900/40 dark:bg-red-900/20 dark:text-red-300">
          <span className="text-muted-foreground">error:</span> {item.error}
        </div>
      )}
      {item.output != null && (
        <div className="mt-1.5">
          <div className="mb-1 text-[10px] text-muted-foreground">output</div>
          <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(item.output)}</pre>
        </div>
      )}
    </InputItemRow>
  )
}

function ItemView({
  item,
  index,
  overlay,
  ctx,
}: {
  item: ResponsesItem
  index: number
  overlay?: CallOverlay | null
  ctx?: OutputCtx
}) {
  switch (item.kind) {
    case "message":
      return <MessageItemView item={item} index={index} overlay={overlay} />
    case "function_call":
      return <FunctionCallItemView item={item} index={index} ctx={ctx} />
    case "function_call_output":
      return (
        <FunctionCallOutputItemView
          item={item}
          index={index}
          renderToolResult={
            overlay?.ToolResultContent
              ? (content, isError) => {
                  const ToolResult = overlay.ToolResultContent!
                  return <ToolResult content={content} isError={isError} />
                }
              : undefined
          }
        />
      )
    case "reasoning":
      return <ReasoningItemView item={item} index={index} />
    case "file_search_call":
      return <FileSearchCallItemView item={item} index={index} />
    case "web_search_call":
      return <WebSearchCallItemView item={item} index={index} />
    case "computer_call":
      return <ComputerCallItemView item={item} index={index} />
    case "mcp_call":
      return <McpCallItemView item={item} index={index} />
    case "unknown":
      return (
        <InputItemRow
          index={index}
          badgeLabel="unknown"
          badgeStyle={ITEM_TYPE_STYLES.unknown}
          badgeTitle="type: unknown"
          preview="unrecognized item"
        >
          <pre className="max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(item.raw)}</pre>
        </InputItemRow>
      )
  }
}

// ── request sections ───────────────────────────────────────────────────────

function InstructionsSection({ instructions }: { instructions: string | null }) {
  const [open, setOpen] = useState(false)
  if (!instructions) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Instructions</span>
        <span className="text-muted-foreground">· {formatSize(byteLength(instructions))}</span>
      </button>
      {open && <div className="px-3 py-2 text-[11px]"><Markdown text={instructions} /></div>}
    </div>
  )
}

function InputItemsSection({ request, overlay, ctx }: { request: ResponsesRequest; overlay?: CallOverlay | null; ctx?: OutputCtx }) {
  const [open, setOpen] = useState(false)
  if (request.input.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full flex-wrap items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Input</span>
        <span className="text-muted-foreground">({request.input.length})</span>
        <ItemTypeChips items={request.input} />
      </button>
      {open && (
        <div>
          {request.input.map((item, i) => <ItemView key={i} item={item} index={i} overlay={overlay} ctx={ctx} />)}
        </div>
      )}
    </div>
  )
}

function ToolsSection({ tools }: { tools: ResponsesToolDef[] }) {
  const [open, setOpen] = useState(false)
  if (tools.length === 0) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Tools</span>
        <span className="text-muted-foreground">({tools.length})</span>
      </button>
      {open && (
        <div className="space-y-2 px-3 py-2">
          {tools.map((t, i) => <ToolDefView key={i} tool={t} />)}
        </div>
      )}
    </div>
  )
}

function ToolDefView({ tool }: { tool: ResponsesToolDef }) {
  return (
    <div className="rounded border border-border/40 bg-muted/20 p-2 text-[11px]">
      <div className="flex items-center gap-2">
        <span className="rounded bg-muted px-1 py-0.5 text-[9px] uppercase tracking-wider">{tool.type}</span>
        {tool.name && <span className="font-medium">{tool.name}</span>}
        {tool.strict && (
          <span className="rounded bg-emerald-100 px-1 py-0.5 text-[9px] text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300">
            strict
          </span>
        )}
      </div>
      {tool.description && <div className="mt-1 text-muted-foreground">{tool.description}</div>}
      {tool.vector_store_ids && (
        <div className="mt-1 text-[10px] text-muted-foreground">
          vector stores: <span className="font-mono">{tool.vector_store_ids.join(", ")}</span>
        </div>
      )}
      {tool.server_label && (
        <div className="mt-1 text-[10px] text-muted-foreground">
          server: <span className="font-mono">{tool.server_label}</span>
          {tool.server_url && <> · <span className="font-mono">{tool.server_url}</span></>}
        </div>
      )}
      {tool.parameters != null && (
        <details className="mt-1">
          <summary className="cursor-pointer text-[10px] text-muted-foreground">parameters</summary>
          <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">
            {formatJson(tool.parameters)}
          </pre>
        </details>
      )}
    </div>
  )
}

function ReasoningConfigSection({ request }: { request: ResponsesRequest }) {
  const r = request.reasoning
  if (!r || (r.effort == null && r.summary == null)) return null
  return (
    <div className="rounded border border-border/60 bg-background px-3 py-2 text-[11px]">
      <span className="font-medium">Reasoning config</span>
      <div className="mt-1 grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5">
        {r.effort && (<><span className="text-muted-foreground">effort</span><span className="font-mono">{r.effort}</span></>)}
        {r.summary && (<><span className="text-muted-foreground">summary</span><span className="font-mono">{r.summary}</span></>)}
      </div>
    </div>
  )
}

function SamplingSection({ request }: { request: ResponsesRequest }) {
  const [open, setOpen] = useState(false)
  const s = request.sampling
  const rows: Array<[string, string | number | null]> = [
    ["model", request.model],
    ["max_output_tokens", s.max_output_tokens],
    ["temperature", s.temperature],
    ["top_p", s.top_p],
    ["stream", s.stream == null ? null : s.stream ? "true" : "false"],
    ["tool_choice", s.tool_choice !== undefined ? (typeof s.tool_choice === "string" ? s.tool_choice : formatJson(s.tool_choice)) : null],
    ["parallel_tool_calls", s.parallel_tool_calls == null ? null : s.parallel_tool_calls ? "true" : "false"],
    ["store", s.store == null ? null : s.store ? "true" : "false"],
    ["truncation", s.truncation],
    ["service_tier", s.service_tier],
    ["user", s.user],
  ]
  const visible = rows.filter(([, v]) => v != null && v !== "")
  if (visible.length === 0 && !s.previous_response_id && s.include.length === 0 && !s.metadata) return null
  return (
    <div className="rounded border border-border/60 bg-background text-xs">
      <button onClick={() => setOpen((o) => !o)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        {open ? <ChevronDown className="size-3 text-muted-foreground" /> : <ChevronRight className="size-3 text-muted-foreground" />}
        <span className="font-medium">Parameters</span>
      </button>
      {open && (
        <div className="space-y-1 px-3 py-2 text-[11px]">
          {s.previous_response_id && (
            <div className="rounded bg-muted/40 px-2 py-1">
              <Link2 className="inline size-2.5 text-muted-foreground" />{" "}
              <span className="text-muted-foreground">continuation of</span>{" "}
              <span className="font-mono text-[10px]">{s.previous_response_id}</span>
            </div>
          )}
          <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5">
            {visible.map(([k, v]) => (
              <div key={k} className="contents">
                <span className="text-muted-foreground">{k}</span>
                <span className="truncate font-mono" title={String(v)}>{String(v)}</span>
              </div>
            ))}
            {s.include.length > 0 && (
              <div className="contents">
                <span className="text-muted-foreground">include</span>
                <span className="font-mono">{s.include.join(", ")}</span>
              </div>
            )}
          </div>
          {s.metadata && (
            <div>
              <span className="text-muted-foreground">metadata</span>
              <pre className="mt-1 max-h-[100px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{formatJson(s.metadata)}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}

// ── response ────────────────────────────────────────────────────────────────

function StatusBadge({ status }: { status: string | null }) {
  if (!status) return null
  const styles: Record<string, string> = {
    completed: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
    incomplete: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    failed: "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300",
    cancelled: "bg-muted text-muted-foreground",
    in_progress: "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  }
  return (
    <span className={cn("rounded px-1.5 py-0.5 text-[10px] font-medium", styles[status] ?? "bg-muted text-muted-foreground")}>
      {status}
    </span>
  )
}

function UsageCard({ response }: { response: ResponsesResponse }) {
  const u = response.usage
  return (
    <div className="rounded border border-border/60 bg-background p-3 text-[11px]">
      <div className="mb-1 flex items-center gap-2">
        <span className="font-medium">Usage</span>
        <StatusBadge status={typeof response.status === "string" ? response.status : null} />
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5">
        <span className="text-muted-foreground">input</span>
        <span className="tabular-nums">{u.input_tokens ?? "—"}</span>
        <span className="text-muted-foreground">output</span>
        <span className="tabular-nums">{u.output_tokens ?? "—"}</span>
        <span className="text-muted-foreground">total</span>
        <span className="tabular-nums">{u.total_tokens ?? "—"}</span>
        {u.cached_input_tokens != null && (
          <>
            <span className="text-muted-foreground">cached_input</span>
            <span className="tabular-nums text-purple-700 dark:text-purple-300">{u.cached_input_tokens}</span>
          </>
        )}
        {u.reasoning_tokens != null && (
          <>
            <span className="text-muted-foreground">reasoning</span>
            <span className="tabular-nums text-purple-700 dark:text-purple-300">{u.reasoning_tokens}</span>
          </>
        )}
        {response.id && (
          <>
            <span className="text-muted-foreground">id</span>
            <span className="font-mono text-[10px]">{response.id}</span>
          </>
        )}
      </div>
    </div>
  )
}

// ── output assistant message (rendered inline, not collapsible) ───────────

// Assistant text in `response.output` is the primary content — render it
// directly like anthropic's TextBlockView rather than wrapping it in the
// InputItemRow collapsible used by request-side items.
function AssistantMessageOutputView({ item }: { item: ResponsesMessageItem }) {
  if (typeof item.content === "string") {
    return <div className="text-[11px]"><Markdown text={item.content} /></div>
  }
  return (
    <div className="space-y-2">
      {item.content.map((p, i) => <ContentPartView key={i} part={p} />)}
    </div>
  )
}

// ── exported views ─────────────────────────────────────────────────────────

export interface OpenAiResponsesCallViewProps {
  requestBody: string | null
  responseBody: string | null
  overlay?: CallOverlay | null
  hasRequestBody: boolean
}

export function OpenAiResponsesCallView({
  requestBody,
  responseBody,
  overlay,
  hasRequestBody,
}: OpenAiResponsesCallViewProps) {
  const call = useMemo(() => parseOpenAiResponsesCall(requestBody, responseBody), [requestBody, responseBody])
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
            <InstructionsSection instructions={call.request.instructions} />
            <InputItemsSection request={call.request} overlay={overlay} />
            <ToolsSection tools={call.request.tools} />
            <ReasoningConfigSection request={call.request} />
            <SamplingSection request={call.request} />
          </div>
        )}
      </section>
      <section className="border-l-2 border-emerald-500/40 pl-3">
        <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-400">Output</div>
        <OpenAiResponsesOutputBlocks response={call.response} overlay={overlay} />
        <div className="mt-2">
          <UsageCard response={call.response} />
        </div>
      </section>
    </>
  )
}

export function OpenAiResponsesOutputBlocks({
  response,
  ctx,
  overlay,
}: {
  response: ResponsesResponse
  ctx?: OutputCtx
  overlay?: CallOverlay | null
}) {
  if (response.output.length === 0) {
    return <div className="text-[11px] text-muted-foreground italic">No response items.</div>
  }
  return (
    <div className="space-y-2">
      {response.output.map((item, i) => {
        if (item.kind === "message" && item.role === "assistant") {
          return <AssistantMessageOutputView key={i} item={item} />
        }
        return <ItemView key={i} item={item} index={i} overlay={overlay} ctx={ctx} />
      })}
    </div>
  )
}

export function openaiResponsesParseForOutput(
  requestBody: string | null | undefined,
  responseBody: string | null | undefined,
) {
  const call = parseOpenAiResponsesCall(requestBody, responseBody)
  return { response: call.response }
}

// ── Input subsection (tool_result back-pointers + optional user text) ──────

export interface OpenAiResponsesParsedInput {
  toolResults: Array<{ call_id: string; content: string }>
  extraUserText: string | null
}

// eslint-disable-next-line react-refresh/only-export-components
export function openaiResponsesParseForInput(requestBody: string | null | undefined): OpenAiResponsesParsedInput {
  if (!requestBody) return { toolResults: [], extraUserText: null }
  const call = parseOpenAiResponsesCall(requestBody, null)
  const items = call.request.input
  let lastCallIdx = -1
  for (let i = items.length - 1; i >= 0; i--) {
    if (items[i].kind === "function_call") { lastCallIdx = i; break }
  }
  const tail = items.slice(lastCallIdx + 1)
  const toolResults: OpenAiResponsesParsedInput["toolResults"] = []
  let extraUserText: string | null = null
  for (const item of tail) {
    if (item.kind === "function_call_output") {
      const content = typeof item.output === "string" ? item.output : formatJson(item.output)
      toolResults.push({ call_id: item.call_id, content })
    } else if (item.kind === "message" && item.role === "user") {
      const txt = typeof item.content === "string"
        ? item.content
        : item.content.map((p) => {
            if (p.type === "input_text" || p.type === "output_text" || p.type === "text") return p.text
            return ""
          }).join("")
      if (txt) extraUserText = txt
    }
  }
  return { toolResults, extraUserText }
}

export function OpenAiResponsesInputBlocks({
  parsed,
  ctx,
}: {
  parsed: OpenAiResponsesParsedInput
  ctx: InputCtx
  overlay?: CallOverlay | null
}) {
  if (parsed.toolResults.length === 0 && !parsed.extraUserText) {
    return <div className="text-[11px] text-muted-foreground italic">No input deltas.</div>
  }
  return (
    <div className="space-y-2">
      {parsed.toolResults.map((tr) => {
        const entry = ctx.toolIndex.get(tr.call_id) ?? { origin: null, resolution: null }
        const state = classifyToolResultState(entry)
        return (
          <div
            key={tr.call_id}
            className={cn(
              "rounded border p-2 text-[11px]",
              state === "orphan"
                ? "bg-amber-50/60 border-amber-200 dark:bg-amber-900/10 dark:border-amber-900/40"
                : "bg-muted/40 border-border/60",
            )}
          >
            <div className="flex items-center gap-2">
              <span className="font-medium">⤷ tool_result</span>
              <span className="font-mono text-[10px] text-muted-foreground">{tr.call_id}</span>
              <span className="text-[10px] text-muted-foreground">· {formatSize(byteLength(tr.content))}</span>
            </div>
            <pre className="mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap font-mono text-[10px]">{tr.content}</pre>
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
