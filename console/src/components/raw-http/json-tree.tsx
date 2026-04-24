import { ChevronRight, ChevronDown } from "lucide-react"
import { collapsedArrayPreview, collapsedObjectPreview } from "./helpers"

type Expansion = Record<string, boolean>

interface JsonTreeProps {
  value: unknown
  expansion: Expansion
  onToggle: (path: string) => void
}

/** Top-level entry: wraps recursion and handles the "not an object/array" edge case. */
export function JsonTree({ value, expansion, onToggle }: JsonTreeProps) {
  return (
    <div className="font-mono text-xs leading-6 text-foreground">
      <JsonNode value={value} path="$" keyLabel={null} expansion={expansion} onToggle={onToggle} />
    </div>
  )
}

interface JsonNodeProps {
  value: unknown
  path: string
  keyLabel: string | null
  expansion: Expansion
  onToggle: (path: string) => void
  isLastSibling?: boolean
}

function JsonNode({ value, path, keyLabel, expansion, onToggle, isLastSibling = true }: JsonNodeProps) {
  const trailing = isLastSibling ? "" : ","

  if (value === null) {
    return (
      <Line keyLabel={keyLabel}>
        <span className="italic text-muted-foreground">null</span>
        {trailing}
      </Line>
    )
  }
  if (typeof value === "string") {
    return (
      <Line keyLabel={keyLabel}>
        <span>&quot;{escapeString(value)}&quot;</span>
        {trailing}
      </Line>
    )
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return (
      <Line keyLabel={keyLabel}>
        <span>{String(value)}</span>
        {trailing}
      </Line>
    )
  }

  const isArray = Array.isArray(value)
  const obj = value as Record<string, unknown> | unknown[]
  const open = expansion[path] === true
  const [openBracket, closeBracket] = isArray ? ["[", "]"] : ["{", "}"]
  const preview = isArray
    ? collapsedArrayPreview(obj as unknown[])
    : collapsedObjectPreview(obj as Record<string, unknown>)

  const entries: [string, unknown, string][] = isArray
    ? (obj as unknown[]).map((v, i) => [String(i), v, `${path}[${i}]`])
    : Object.keys(obj as Record<string, unknown>).map((k) => [k, (obj as Record<string, unknown>)[k], `${path}.${k}`])

  if (!open) {
    return (
      <Line keyLabel={keyLabel}>
        <button
          type="button"
          onClick={() => onToggle(path)}
          className="inline-flex items-center gap-1 hover:text-foreground"
        >
          <ChevronRight className="size-3 text-muted-foreground" />
          <span className="text-muted-foreground">{preview}</span>
        </button>
        {trailing}
      </Line>
    )
  }

  return (
    <>
      <Line keyLabel={keyLabel}>
        <button
          type="button"
          onClick={() => onToggle(path)}
          className="inline-flex items-center gap-1 hover:text-foreground"
        >
          <ChevronDown className="size-3 text-muted-foreground" />
          <span>{openBracket}</span>
        </button>
      </Line>
      <div className="pl-4">
        {entries.map(([childKey, childVal, childPath], idx) => (
          <JsonNode
            key={childPath}
            value={childVal}
            path={childPath}
            keyLabel={isArray ? null : childKey}
            expansion={expansion}
            onToggle={onToggle}
            isLastSibling={idx === entries.length - 1}
          />
        ))}
      </div>
      <Line keyLabel={null}>
        {closeBracket}
        {trailing}
      </Line>
    </>
  )
}

function Line({ keyLabel, children }: { keyLabel: string | null; children: React.ReactNode }) {
  return (
    <div className="whitespace-pre">
      {keyLabel != null && <span className="text-sky-400">{keyLabel}</span>}
      {keyLabel != null && <span className="text-muted-foreground">: </span>}
      {children}
    </div>
  )
}

/** Minimal string escape for display. Keeps newlines visible as \n rather than breaking the line. */
function escapeString(s: string): string {
  return s
    .replace(/\\/g, "\\\\")
    .replace(/"/g, '\\"')
    .replace(/\n/g, "\\n")
    .replace(/\t/g, "\\t")
}
