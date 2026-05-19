import { useState, type KeyboardEvent } from "react"
import { X } from "lucide-react"

/**
 * Lightweight chip input for the structured BPF editor. Tokens are added
 * on Enter / comma / blur and removed via the × button or Backspace on
 * an empty input.
 */
export function ChipInput({
  values,
  onChange,
  placeholder,
  validate,
}: {
  values: string[]
  onChange: (next: string[]) => void
  placeholder?: string
  /** Optional per-token validator. Invalid tokens are accepted but shown red. */
  validate?: (token: string) => boolean
}) {
  const [draft, setDraft] = useState("")

  const commit = (raw: string) => {
    const cleaned = raw.trim().replace(/,$/, "").trim()
    if (cleaned === "") return
    if (values.includes(cleaned)) {
      setDraft("")
      return
    }
    onChange([...values, cleaned])
    setDraft("")
  }

  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault()
      commit(draft)
    } else if (e.key === "Backspace" && draft === "" && values.length > 0) {
      e.preventDefault()
      onChange(values.slice(0, -1))
    }
  }

  return (
    <div className="flex min-h-[28px] flex-wrap items-center gap-1.5 rounded-md border border-border bg-background px-2 py-1">
      {values.map((v, i) => {
        const ok = validate ? validate(v) : true
        return (
          <span
            key={`${v}-${i}`}
            className={
              ok
                ? "inline-flex items-center gap-1 rounded bg-muted px-1.5 py-0.5 font-mono text-xs"
                : "inline-flex items-center gap-1 rounded bg-destructive/10 px-1.5 py-0.5 font-mono text-xs text-destructive"
            }
          >
            {v}
            <button
              type="button"
              onClick={() => onChange(values.filter((_, idx) => idx !== i))}
              className="opacity-60 hover:opacity-100"
              aria-label={`Remove ${v}`}
            >
              <X className="size-3" />
            </button>
          </span>
        )
      })}
      <input
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => commit(draft)}
        onKeyDown={onKey}
        placeholder={values.length === 0 ? placeholder : ""}
        className="min-w-[120px] flex-1 bg-transparent px-1 py-0.5 text-xs outline-none placeholder:text-muted-foreground"
      />
    </div>
  )
}
