import { useState, useRef, useEffect } from "react"
import { ChevronDown, X } from "lucide-react"
import { cn } from "@/lib/utils"

interface Props {
  label: string
  options: string[]
  selected: string[]
  onChange: (selected: string[]) => void
}

export function FilterDropdown({ label, options, selected, onChange }: Props) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    function handleClick(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false)
      }
    }
    document.addEventListener("mousedown", handleClick)
    return () => document.removeEventListener("mousedown", handleClick)
  }, [open])

  function toggle(value: string) {
    if (selected.includes(value)) {
      onChange(selected.filter((v) => v !== value))
    } else {
      onChange([...selected, value])
    }
  }

  function clear() {
    onChange([])
  }

  const hasSelection = selected.length > 0

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen(!open)}
        className={cn(
          "flex items-center gap-1.5 rounded-lg border px-2.5 py-1.5 text-xs transition-colors hover:bg-muted",
          hasSelection
            ? "border-foreground/20 bg-muted font-medium"
            : "border-border text-muted-foreground",
          open && "bg-muted",
        )}
      >
        <span>{label}</span>
        {hasSelection && (
          <span className="flex size-4 items-center justify-center rounded-full bg-foreground text-[10px] font-semibold text-background">
            {selected.length}
          </span>
        )}
        {hasSelection ? (
          <X
            className="size-3 text-muted-foreground hover:text-foreground"
            onClick={(e) => {
              e.stopPropagation()
              clear()
            }}
          />
        ) : (
          <ChevronDown className="size-3 text-muted-foreground" />
        )}
      </button>

      {open && (
        <div className="absolute left-0 top-full z-50 mt-1 min-w-[180px] rounded-lg border border-border bg-background p-1 shadow-lg">
          {options.length === 0 ? (
            <div className="px-3 py-2 text-xs text-muted-foreground">No options</div>
          ) : (
            <div className="max-h-[240px] overflow-auto">
              {options.map((opt) => {
                const isChecked = selected.includes(opt)
                return (
                  <button
                    key={opt}
                    onClick={() => toggle(opt)}
                    className={cn(
                      "flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-xs transition-colors hover:bg-muted",
                      isChecked && "font-medium",
                    )}
                  >
                    <div
                      className={cn(
                        "flex size-3.5 shrink-0 items-center justify-center rounded border",
                        isChecked
                          ? "border-foreground bg-foreground text-background"
                          : "border-border",
                      )}
                    >
                      {isChecked && (
                        <svg width="8" height="8" viewBox="0 0 8 8" fill="none">
                          <path d="M1 4L3 6L7 2" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                        </svg>
                      )}
                    </div>
                    <span className="truncate" title={opt}>
                      {opt}
                    </span>
                  </button>
                )
              })}
            </div>
          )}
        </div>
      )}
    </div>
  )
}
