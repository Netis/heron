import { useState } from "react"
import { Info, Loader2 } from "lucide-react"
import { useInternalMetrics } from "@/hooks/use-internal-metrics"
import { usePipelineHealthStore } from "@/stores/pipeline-health"
import { classifyHealth } from "@/lib/pipeline-health"
import { HealthPill } from "@/components/pipeline-health/health-pill"
import { BackpressureSection } from "@/components/pipeline-health/backpressure-section"
import { FunnelSection } from "@/components/pipeline-health/funnel-section"
import { StateGaugesSection } from "@/components/pipeline-health/state-gauges-section"
import { ErrorListSection } from "@/components/pipeline-health/error-list-section"
import { AllMetricsTable } from "@/components/pipeline-health/all-metrics-table"

/**
 * Developer-only pipeline diagnostics page. Reachable from the `/debug`
 * index — intentionally not advertised in the sidebar.
 */
export function PipelineHealthPage() {
  const { data, isLoading } = useInternalMetrics()
  const intervalMs = usePipelineHealthStore((s) => s.intervalMs)
  const setIntervalMs = usePipelineHealthStore((s) => s.setIntervalMs)
  const selectedPipeline = usePipelineHealthStore((s) => s.selectedPipeline)
  const setSelectedPipeline = usePipelineHealthStore(
    (s) => s.setSelectedPipeline,
  )

  // Track current and previous frame's metric snapshots for delta display.
  // Uses the "store info from previous renders" pattern: roll the snapshot
  // during render when `data.ts` advances. See React docs:
  // https://react.dev/reference/react/useState#storing-information-from-previous-renders
  // - `currByName`/`currTs` mirror the most recently absorbed frame.
  // - `prevByName`/`prevTs` are what `currByName`/`currTs` were before the
  //   last roll; they are what child components consume as "previous".
  const [tracked, setTracked] = useState<{
    currByName: Record<string, number>
    currTs: number | null
    prevByName: Record<string, number>
    prevTs: number | null
  }>({ currByName: {}, currTs: null, prevByName: {}, prevTs: null })
  if (data && data.ts !== tracked.currTs) {
    const nextByName: Record<string, number> = {}
    for (const p of data.pipelines) for (const m of p.metrics) nextByName[m.name] = m.value
    for (const m of data.global.metrics) nextByName[m.name] = m.value
    setTracked((t) => ({
      currByName: nextByName,
      currTs: data.ts,
      prevByName: t.currByName,
      prevTs: t.currTs,
    }))
  }
  const prevByName = tracked.prevByName
  const prevTs = tracked.prevTs

  if (isLoading || !data) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  const pipelineNames = data.pipelines.map((p) => p.name)
  const activeName =
    selectedPipeline && pipelineNames.includes(selectedPipeline)
      ? selectedPipeline
      : (pipelineNames[0] ?? null)
  const active = data.pipelines.find((p) => p.name === activeName)
  const hasPipelines = pipelineNames.length > 0

  const allMetrics = [
    ...(active?.metrics ?? []),
    ...data.global.metrics,
  ]
  // First frame has no prev — pass empty map so delta-based rules sit quiet
  // until the second frame.
  const health = classifyHealth(allMetrics, {})

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* ===== Header ===== */}
      <div className="flex items-center gap-3 rounded-lg border border-border bg-card p-3">
        <span className="text-sm font-semibold">Pipeline Health</span>

        {hasPipelines && pipelineNames.length > 1 ? (
          <select
            className="h-7 rounded-md border border-input bg-background px-2 text-xs"
            value={activeName ?? ""}
            onChange={(e) => setSelectedPipeline(e.target.value || null)}
          >
            {pipelineNames.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        ) : hasPipelines ? (
          <span className="rounded-md bg-muted px-2 py-0.5 text-xs text-muted-foreground">
            {activeName}
          </span>
        ) : null}

        <HealthPill level={health.level} count={health.findings.length} />

        <div className="ml-auto flex items-center gap-1">
          {[1000, 2000, 5000, null].map((ms) => (
            <button
              key={String(ms)}
              onClick={() => setIntervalMs(ms)}
              className={`h-7 rounded-md px-2 text-xs ${
                intervalMs === ms
                  ? "bg-foreground text-background"
                  : "bg-muted text-muted-foreground hover:bg-muted/70"
              }`}
            >
              {ms === null ? "Pause" : `${ms / 1000}s`}
            </button>
          ))}
        </div>
      </div>

      {/* ===== Sections ===== */}
      {hasPipelines ? (
        <>
          <BackpressureSection
            pipelineMetrics={active?.metrics ?? []}
            globalMetrics={data.global.metrics}
          />
          <FunnelSection
            pipelineMetrics={active?.metrics ?? []}
            globalMetrics={data.global.metrics}
          />
          <StateGaugesSection
            pipelineMetrics={active?.metrics ?? []}
            globalMetrics={data.global.metrics}
          />
          <ErrorListSection
            pipelineMetrics={active?.metrics ?? []}
            globalMetrics={data.global.metrics}
            prevByName={prevByName}
          />
        </>
      ) : (
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="flex items-start gap-3">
            <Info className="mt-0.5 size-5 shrink-0 text-muted-foreground" />
            <div className="flex flex-col gap-2">
              <span className="text-sm font-semibold">No active pipelines</span>
              <p className="text-sm text-muted-foreground">
                Pipeline-level metrics are unavailable because no capture
                pipelines are running. Typical causes:
              </p>
              <ul className="ml-4 list-disc text-sm text-muted-foreground">
                <li>No pcap source or ZMQ ingest configured</li>
                <li>
                  Pcap file finished and TokenScope is staying up (
                  <code className="rounded bg-muted px-1 py-0.5 text-xs">
                    --exit-after-drain
                  </code>{" "}
                  disabled)
                </li>
                <li>All sources stopped</li>
              </ul>
              <p className="text-sm text-muted-foreground">
                Global metrics are still shown below for diagnostics.
              </p>
            </div>
          </div>
        </div>
      )}

      <AllMetricsTable
        pipelineMetrics={active?.metrics ?? []}
        globalMetrics={data.global.metrics}
        prevByName={prevByName}
        ts={data.ts}
        prevTs={prevTs}
      />
    </div>
  )
}
