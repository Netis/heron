import { useState, useRef, useEffect } from "react"
import { Calendar, ChevronDown, RefreshCw } from "lucide-react"
import { useIsFetching } from "@tanstack/react-query"
import { useToolbarStore, type TimeRangePreset } from "@/stores/toolbar"
import { useWireApis, useModelNames, useServerIps } from "@/hooks/use-filter-values"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import { cn } from "@/lib/utils"

const presets: { value: Exclude<TimeRangePreset, "custom">; label: string }[] = [
  { value: "5m", label: "5m" },
  { value: "15m", label: "15m" },
  { value: "1h", label: "1h" },
  { value: "6h", label: "6h" },
  { value: "24h", label: "24h" },
  { value: "7d", label: "7d" },
]

function epochToLocalDatetime(epoch: number): string {
  const d = new Date(epoch * 1000)
  const pad = (n: number) => String(n).padStart(2, "0")
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function localDatetimeToEpoch(dt: string): number {
  return Math.floor(new Date(dt).getTime() / 1000)
}

function formatRangeLabel(start: number, end: number): string {
  const fmt = (epoch: number) => {
    const d = new Date(epoch * 1000)
    const pad = (n: number) => String(n).padStart(2, "0")
    const month = pad(d.getMonth() + 1)
    const day = pad(d.getDate())
    const hh = pad(d.getHours())
    const mm = pad(d.getMinutes())
    return `${month}-${day} ${hh}:${mm}`
  }
  return `${fmt(start)} ~ ${fmt(end)}`
}

const refreshIntervals: { value: number; label: string }[] = [
  { value: 0, label: "Off" },
  { value: 5000, label: "5s" },
  { value: 10000, label: "10s" },
  { value: 30000, label: "30s" },
  { value: 60000, label: "1m" },
]

function formatRefreshLabel(ms: number): string {
  const item = refreshIntervals.find((r) => r.value === ms)
  return item?.label ?? "Off"
}

function csvToArray(csv: string): string[] {
  return csv ? csv.split(",") : []
}

function arrayToCsv(arr: string[]): string {
  return arr.join(",")
}

export function Toolbar() {
  const { preset, start, end, filters, refreshInterval, setPreset, setCustomRange, setFilter, setRefreshInterval } = useToolbarStore()
  const [open, setOpen] = useState(false)
  const [customStart, setCustomStart] = useState("")
  const [customEnd, setCustomEnd] = useState("")
  const dropdownRef = useRef<HTMLDivElement>(null)

  const isFetching = useIsFetching()

  const { data: wireApisData } = useWireApis()
  const { data: modelsData } = useModelNames()
  const { data: serverIpsData } = useServerIps()

  // Close dropdown on outside click
  useEffect(() => {
    if (!open) return
    function handleClick(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setOpen(false)
      }
    }
    document.addEventListener("mousedown", handleClick)
    return () => document.removeEventListener("mousedown", handleClick)
  }, [open])

  // Sync custom inputs when dropdown opens
  useEffect(() => {
    if (open) {
      setCustomStart(epochToLocalDatetime(start))
      setCustomEnd(epochToLocalDatetime(end))
    }
  }, [open, start, end])

  const handlePresetClick = (value: Exclude<TimeRangePreset, "custom">) => {
    setPreset(value)
    setOpen(false)
  }

  const handleApplyCustom = () => {
    const s = localDatetimeToEpoch(customStart)
    const e = localDatetimeToEpoch(customEnd)
    if (s && e && s < e) {
      setCustomRange(s, e)
      setOpen(false)
    }
  }

  return (
    <header className="flex h-12 shrink-0 items-center gap-4 border-b border-border bg-background px-4">
      <div className="relative" ref={dropdownRef}>
        <button
          onClick={() => setOpen(!open)}
          className={cn(
            "flex items-center gap-2 rounded-lg border border-border px-3 py-1.5 text-sm transition-colors hover:bg-muted",
            open && "bg-muted",
          )}
        >
          <Calendar className="size-3.5 text-muted-foreground" />
          {preset === "custom" ? (
            <span className="tabular-nums">{formatRangeLabel(start, end)}</span>
          ) : (
            <span>Last {preset}</span>
          )}
          {refreshInterval > 0 && preset !== "custom" && (
            <span className="flex items-center gap-1 text-xs text-muted-foreground">
              <RefreshCw className={cn("size-3", isFetching > 0 && "animate-spin")} />
              {formatRefreshLabel(refreshInterval)}
            </span>
          )}
          <ChevronDown className="size-3.5 text-muted-foreground" />
        </button>

        {open && (
          <div className="absolute left-0 top-full z-50 mt-1 w-[340px] rounded-lg border border-border bg-background p-3 shadow-lg">
            {/* Quick presets */}
            <div className="mb-3">
              <div className="mb-1.5 text-xs font-medium text-muted-foreground">Quick Select</div>
              <div className="flex flex-wrap gap-1">
                {presets.map((p) => (
                  <button
                    key={p.value}
                    onClick={() => handlePresetClick(p.value)}
                    className={cn(
                      "rounded-md px-3 py-1.5 text-xs font-medium transition-colors",
                      preset === p.value
                        ? "bg-foreground text-background"
                        : "bg-muted text-muted-foreground hover:text-foreground",
                    )}
                  >
                    Last {p.label}
                  </button>
                ))}
              </div>
            </div>

            {/* Custom range */}
            <div className="border-t border-border pt-3">
              <div className="mb-1.5 text-xs font-medium text-muted-foreground">Custom Range</div>
              <div className="flex flex-col gap-2">
                <div className="flex items-center gap-2">
                  <label className="w-10 text-xs text-muted-foreground">From</label>
                  <input
                    type="datetime-local"
                    value={customStart}
                    onChange={(e) => setCustomStart(e.target.value)}
                    className="flex-1 rounded-md border border-border bg-background px-2 py-1.5 text-xs tabular-nums"
                  />
                </div>
                <div className="flex items-center gap-2">
                  <label className="w-10 text-xs text-muted-foreground">To</label>
                  <input
                    type="datetime-local"
                    value={customEnd}
                    onChange={(e) => setCustomEnd(e.target.value)}
                    className="flex-1 rounded-md border border-border bg-background px-2 py-1.5 text-xs tabular-nums"
                  />
                </div>
                <button
                  onClick={handleApplyCustom}
                  className="mt-1 rounded-md bg-foreground px-3 py-1.5 text-xs font-medium text-background transition-colors hover:bg-foreground/90"
                >
                  Apply
                </button>
              </div>
            </div>

            {/* Auto Refresh */}
            <div className={cn("border-t border-border pt-3", preset === "custom" && "opacity-40")}>
              <div className="mb-1.5 text-xs font-medium text-muted-foreground">Auto Refresh</div>
              <div className="flex flex-wrap gap-1">
                {refreshIntervals.map((r) => (
                  <button
                    key={r.value}
                    disabled={preset === "custom"}
                    onClick={() => setRefreshInterval(r.value)}
                    className={cn(
                      "rounded-md px-3 py-1.5 text-xs font-medium transition-colors",
                      refreshInterval === r.value
                        ? "bg-foreground text-background"
                        : "bg-muted text-muted-foreground hover:text-foreground",
                      preset === "custom" && "cursor-not-allowed",
                    )}
                  >
                    {r.label}
                  </button>
                ))}
              </div>
              {preset === "custom" && (
                <p className="mt-1.5 text-[10px] text-muted-foreground">
                  Auto refresh is only available with relative time ranges
                </p>
              )}
            </div>
          </div>
        )}
      </div>

      {/* Dimension filters */}
      <FilterDropdown
        label="Wire API"
        options={wireApisData?.values ?? []}
        selected={csvToArray(filters.wireApi)}
        onChange={(v) => setFilter("wireApi", arrayToCsv(v))}
      />
      <FilterDropdown
        label="Model"
        options={modelsData?.values ?? []}
        selected={csvToArray(filters.model)}
        onChange={(v) => setFilter("model", arrayToCsv(v))}
      />
      <FilterDropdown
        label="Server IP"
        options={serverIpsData?.values ?? []}
        selected={csvToArray(filters.serverIp)}
        onChange={(v) => setFilter("serverIp", arrayToCsv(v))}
      />
    </header>
  )
}
