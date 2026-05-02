import { useState, useEffect } from "react"
import { X } from "lucide-react"
import {
  type ExtractFormValues,
  type Anchor,
  defaultsFor,
  validate,
  buildExtractUrl,
} from "./extract-defaults"

interface Props {
  anchor: Anchor
  open: boolean
  onClose: () => void
}

function usToInputLocal(us: number): string {
  // datetime-local needs "YYYY-MM-DDTHH:MM:SS" in *local* time (no Z).
  const ms = Math.round(us / 1000)
  const d = new Date(ms)
  const pad = (n: number) => n.toString().padStart(2, "0")
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}
function inputLocalToUs(s: string): number {
  return new Date(s).getTime() * 1000
}

/// Stable identity for an anchor — used as the effect dep so auto-refresh
/// (which produces a fresh `anchor.row` reference every interval) doesn't
/// clobber the user's in-flight form edits. Only true row changes
/// (different ID) trigger a defaults reset.
function anchorKey(a: Anchor): string {
  return a.type === "agent_turn"
    ? `agent_turn:${a.row.turn_id}`
    : `${a.type}:${a.row.id}`
}

export function ExtractDialog({ anchor, open, onClose }: Props) {
  const [values, setValues] = useState<ExtractFormValues>(() => defaultsFor(anchor))
  const key = anchorKey(anchor)

  // Reset only when the dialog opens (false→true) or the user navigates
  // to a different row. New `anchor` references with the same `key` —
  // produced by TanStack Query's auto-refresh — preserve user edits.
  useEffect(() => {
    if (open) setValues(defaultsFor(anchor))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, key])

  if (!open) return null

  const v = validate(values)

  const onExtract = () => {
    if (!v.ok) return
    const a = document.createElement("a")
    a.href = buildExtractUrl(values)
    a.download = ""    // honor Content-Disposition filename
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    onClose()
  }

  return (
    <>
      <div className="fixed inset-0 z-50 bg-black/30" onClick={onClose} />
      <div className="fixed left-1/2 top-1/2 z-50 w-[480px] -translate-x-1/2 -translate-y-1/2 rounded-md border border-border bg-background p-4 shadow-xl">
        <div className="mb-3 flex items-center justify-between">
          <h3 className="text-sm font-semibold">Extract packets</h3>
          <button onClick={onClose} className="rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground">
            <X className="size-4" />
          </button>
        </div>

        <div className="grid grid-cols-[120px_1fr] gap-y-2 text-xs">
          <Label>source_id</Label>
          <input value={values.source_id} readOnly className="rounded border border-border bg-muted px-2 py-1" />

          <Label>client_ip</Label>
          <input value={values.client_ip} onChange={(e) => setValues({ ...values, client_ip: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>client_port</Label>
          <input value={values.client_port} onChange={(e) => setValues({ ...values, client_port: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>server_ip</Label>
          <input value={values.server_ip} onChange={(e) => setValues({ ...values, server_ip: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>server_port</Label>
          <input value={values.server_port} onChange={(e) => setValues({ ...values, server_port: e.target.value })} className={inputCls} placeholder="(any)" />

          <Label>start (local)</Label>
          <input type="datetime-local" step="1" value={usToInputLocal(values.start_us)} onChange={(e) => setValues({ ...values, start_us: inputLocalToUs(e.target.value) })} className={inputCls} />

          <Label>end (local)</Label>
          <input type="datetime-local" step="1" value={usToInputLocal(values.end_us)} onChange={(e) => setValues({ ...values, end_us: inputLocalToUs(e.target.value) })} className={inputCls} />
        </div>

        {!v.ok && <p className="mt-3 text-xs text-red-500">{v.reason}</p>}

        <div className="mt-4 flex justify-end gap-2">
          <button onClick={onClose} className="rounded-md border border-border px-3 py-1 text-xs hover:bg-muted">Cancel</button>
          <button onClick={onExtract} disabled={!v.ok} className="rounded-md bg-primary px-3 py-1 text-xs text-primary-foreground hover:bg-primary/90 disabled:opacity-50">Extract</button>
        </div>
      </div>
    </>
  )
}

const inputCls = "rounded border border-border bg-background px-2 py-1"
function Label({ children }: { children: React.ReactNode }) {
  return <label className="self-center pr-2 text-muted-foreground">{children}</label>
}
