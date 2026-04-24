import { useMemo, useState } from "react"
import { ChevronDown, ChevronRight, Copy, Maximize2, Minimize2 } from "lucide-react"
import { JsonTree } from "./json-tree"
import { defaultExpansion, formatJson, formatSize, tryParseJson, walkAllPaths } from "./helpers"

const MODE_KEY = "tokenscope.rawHttp.bodyMode"
const TREE_SIZE_LIMIT = 500 * 1024

type Mode = "raw" | "tree"

function loadMode(): Mode {
  if (typeof window === "undefined") return "tree"
  const v = window.localStorage.getItem(MODE_KEY)
  return v === "raw" ? "raw" : "tree"
}
function saveMode(m: Mode): void {
  if (typeof window === "undefined") return
  window.localStorage.setItem(MODE_KEY, m)
}

interface BodyViewerProps {
  title: string
  raw: string | null
  defaultOpen?: boolean
}

export function BodyViewer({ title, raw, defaultOpen = true }: BodyViewerProps) {
  const [open, setOpen] = useState(defaultOpen)
  const [mode, setMode] = useState<Mode>(() => loadMode())

  const parsed = useMemo(() => tryParseJson(raw), [raw])
  const isJson = parsed !== undefined
  const oversize = (raw?.length ?? 0) > TREE_SIZE_LIMIT

  const effectiveMode: Mode = isJson && !oversize ? mode : "raw"

  // Reset expansion when the underlying body changes (new call opened).
  // Updating state during render is the React-recommended pattern for
  // "derive state from prop" — see https://react.dev/reference/react/useState#storing-information-from-previous-renders
  const [parsedSnapshot, setParsedSnapshot] = useState<unknown>(parsed)
  const [expansion, setExpansion] = useState<Record<string, boolean>>(() =>
    parsed !== undefined ? defaultExpansion(parsed) : {},
  )
  if (parsed !== parsedSnapshot) {
    setParsedSnapshot(parsed)
    setExpansion(parsed !== undefined ? defaultExpansion(parsed) : {})
  }

  const pretty = useMemo(() => formatJson(raw), [raw])
  const size = useMemo(() => formatSize(raw), [raw])

  const empty = !raw
  const ChevronIcon = open ? ChevronDown : ChevronRight

  const onToggleNode = (p: string) => {
    setExpansion((prev) => ({ ...prev, [p]: !prev[p] }))
  }
  const onExpandAll = () => {
    if (parsed === undefined) return
    const paths = walkAllPaths(parsed)
    const next: Record<string, boolean> = {}
    for (const p of paths) next[p] = true
    setExpansion(next)
  }
  const onCollapseAll = () => {
    setExpansion({ $: true })
  }
  const onCopy = () => {
    if (!raw) return
    void navigator.clipboard.writeText(pretty)
  }
  const onModeChange = (m: Mode) => {
    setMode(m)
    saveMode(m)
  }

  return (
    <div className="border-t border-border">
      <div className="flex items-center gap-2 px-4 py-2.5">
        <button
          type="button"
          onClick={() => setOpen(!open)}
          className="flex items-center gap-2 text-sm font-medium hover:text-foreground"
        >
          <ChevronIcon className="size-4 text-muted-foreground" />
          <span>{title}</span>
          <span className="text-xs text-muted-foreground">· {size}</span>
        </button>
        <div className="ml-auto flex items-center gap-1">
          {!empty && isJson && !oversize && (
            <ModeToggle mode={effectiveMode} onChange={onModeChange} />
          )}
          {!empty && effectiveMode === "tree" && (
            <>
              <IconButton title="Expand all" onClick={onExpandAll}>
                <Maximize2 className="size-3.5" />
              </IconButton>
              <IconButton title="Collapse all" onClick={onCollapseAll}>
                <Minimize2 className="size-3.5" />
              </IconButton>
            </>
          )}
          {!empty && (
            <IconButton title="Copy" onClick={onCopy}>
              <Copy className="size-3.5" />
            </IconButton>
          )}
        </div>
      </div>
      {open && (
        <div className="px-4 pb-4">
          {empty ? (
            <p className="text-sm text-muted-foreground">No body</p>
          ) : effectiveMode === "raw" ? (
            <>
              {!isJson && (
                <p className="mb-2 text-xs text-muted-foreground">
                  Not valid JSON — showing raw text.
                </p>
              )}
              {oversize && isJson && (
                <p className="mb-2 text-xs text-muted-foreground">
                  Tree mode disabled for body &gt; 500 KB.
                </p>
              )}
              <pre className="max-h-[60vh] overflow-auto rounded-md bg-muted p-3 font-mono text-xs">
                {pretty}
              </pre>
            </>
          ) : (
            <div className="max-h-[60vh] overflow-auto rounded-md bg-muted p-3">
              <JsonTree value={parsed} expansion={expansion} onToggle={onToggleNode} />
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function ModeToggle({ mode, onChange }: { mode: Mode; onChange: (m: Mode) => void }) {
  return (
    <div className="flex overflow-hidden rounded-md border border-border text-[11px]">
      <button
        type="button"
        onClick={() => onChange("raw")}
        className={
          mode === "raw"
            ? "bg-muted px-2 py-0.5 text-foreground"
            : "px-2 py-0.5 text-muted-foreground hover:text-foreground"
        }
      >
        Raw
      </button>
      <button
        type="button"
        onClick={() => onChange("tree")}
        className={
          mode === "tree"
            ? "bg-muted px-2 py-0.5 text-foreground"
            : "px-2 py-0.5 text-muted-foreground hover:text-foreground"
        }
      >
        Tree
      </button>
    </div>
  )
}

function IconButton({
  title,
  onClick,
  children,
}: { title: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      className="rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground"
    >
      {children}
    </button>
  )
}
