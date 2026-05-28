import { Loader2, Activity, Zap, ShieldAlert, Cpu, Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { formatMs, formatNumber } from "@/lib/format"
import { useMetricsSummary, useTimeseries, useModels } from "@/hooks/use-metrics"
import { RequestVolumeChart } from "@/components/charts/request-volume-chart"
import { LatencyOverviewChart } from "@/components/charts/latency-overview-chart"
import { ModelBreakdownChart } from "@/components/charts/model-breakdown-chart"
import { ErrorByModelChart } from "@/components/charts/error-by-model-chart"
import { LabPanel } from "@/components/lab/LabPanel"
import { NeuralPropagation } from "@/components/lab/NeuralPropagation"
import { ImperialSeal } from "@/components/lab/ImperialSeal"

function LabKpi({
  title,
  value,
  subtext,
  icon: Icon,
  color = "cyan",
}: {
  title: string
  value: string
  subtext?: string
  icon: any
  color?: "cyan" | "emerald" | "rose"
}) {
  const accentColor = 
    color === "emerald" ? "text-emerald-400 group-hover:text-emerald-300" :
    color === "rose" ? "text-rose-400 group-hover:text-rose-300" :
    "text-cyan-400 group-hover:text-cyan-300"

  return (
    <LabPanel className="group">
      <div className="flex items-start justify-between mb-2">
        <span className="text-[10px] font-bold tracking-widest uppercase text-muted-foreground/60">{title}</span>
        <Icon className={cn("size-3 hidden sm:block", accentColor)} />
      </div>
      <div className={cn("text-2xl font-mono tracking-tight", accentColor)}>{value}</div>
      {subtext && <div className="text-[10px] text-muted-foreground/40 mt-1 font-mono">{subtext}</div>}
    </LabPanel>
  )
}

export function OverviewPage() {
  const { data: summary, isLoading: summaryLoading } = useMetricsSummary()
  const { data: volumeTs } = useTimeseries("request_count", { groupBy: "wire_api" })
  const { data: latencyTs } = useTimeseries("ttfb_avg,ttfb_p95,e2e_avg,e2e_p95")
  const { data: modelsData } = useModels()

  if (summaryLoading) {
    return (
      <div className="flex h-full items-center justify-center bg-background lab-scanline">
        <Loader2 className="size-6 animate-spin text-primary" />
      </div>
    )
  }

  const errorRate =
    summary && summary.request_count > 0
      ? (summary.error_count / summary.request_count) * 100
      : 0

  const totalTokens = (summary?.total_input_tokens ?? 0) + (summary?.total_output_tokens ?? 0)

  return (
    <div className="flex flex-col gap-6 p-6 min-h-full bg-background lab-scanline overflow-x-hidden">
      {/* Top Section: Hero Visualization */}
      <div className="grid grid-cols-1 lg:grid-cols-12 gap-6 items-stretch">
        <LabPanel 
          title="Neural Token Propagation Flow" 
          status="online"
          className="lg:col-span-8 h-[320px]"
          headerExtra={<span className="text-[10px] font-mono text-emerald-500/50 tracking-tighter">SIG_INT_L4_ACTIVE</span>}
        >
          <NeuralPropagation />
        </LabPanel>

        <div className="lg:col-span-4 flex flex-col gap-4 justify-between">
          <LabPanel title="System Status" className="flex-1">
             <div className="flex flex-col items-center justify-center h-full gap-4 relative">
                <ImperialSeal size={72} className="opacity-80" />
                <div className="text-center">
                   <div className="text-[10px] font-bold text-muted-foreground uppercase tracking-[0.3em]">Integrity Level</div>
                   <div className="text-xl font-mono text-emerald-400">NOMINAL</div>
                </div>
                {/* Micro metrics */}
                <div className="w-full flex justify-around border-t border-white/5 pt-4 mt-2">
                   <div className="text-center">
                      <div className="text-[9px] text-muted-foreground/50">CPU</div>
                      <div className="text-xs font-mono">14.2%</div>
                   </div>
                   <div className="text-center border-l border-white/5 pl-4">
                      <div className="text-[9px] text-muted-foreground/50">MEM</div>
                      <div className="text-xs font-mono">2.8 GB</div>
                   </div>
                </div>
             </div>
          </LabPanel>
        </div>
      </div>

      {/* KPI Lab Grid */}
      <div className="grid grid-cols-2 md:grid-cols-3 lg:grid-cols-6 gap-4">
        <LabKpi
          title="Throughput"
          value={formatNumber(summary?.request_count ?? 0)}
          icon={Activity}
          subtext="Total Requests"
        />
        <LabKpi
          title="TTFB Avg"
          value={formatMs(summary?.ttfb_avg)}
          icon={Zap}
          color="emerald"
        />
        <LabKpi
          title="E2E Peak"
          value={formatMs(summary?.e2e_avg)}
          icon={Cpu}
        />
        <LabKpi
          title="Anomalies"
          value={`${errorRate.toFixed(2)}%`}
          icon={ShieldAlert}
          color={errorRate > 5 ? "rose" : "emerald"}
          subtext="Error Frequency"
        />
        <LabKpi
          title="Vector Flow"
          value={formatNumber(totalTokens)}
          icon={Layers}
          subtext={`${formatNumber(summary?.total_input_tokens)} in / ${formatNumber(summary?.total_output_tokens)} out`}
        />
        <LabKpi
          title="Efficiency"
          value={summary?.tpot_avg != null ? `${summary.tpot_avg.toFixed(1)} ms/t` : "—"}
          icon={Activity}
          color="emerald"
          subtext="Streaming avg"
        />
      </div>

      {/* Analytics Section */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <LabPanel title="Volumetric Analysis" headerExtra={<div className="h-1 w-20 bg-cyan-500/20 rounded-full overflow-hidden self-center"><div className="h-full bg-cyan-500 w-[60%]" /></div>}>
          <div className="h-[200px] mt-2">
             <RequestVolumeChart data={volumeTs ?? null} />
          </div>
        </LabPanel>
        <LabPanel title="Temporal Forensics" headerExtra={<div className="h-1 w-20 bg-emerald-500/20 rounded-full overflow-hidden self-center"><div className="h-full bg-emerald-500 w-[40%]" /></div>}>
          <div className="h-[200px] mt-2">
             <LatencyOverviewChart data={latencyTs ?? null} />
          </div>
        </LabPanel>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <LabPanel title="Model Distribution">
           <div className="h-[240px]">
              <ModelBreakdownChart models={modelsData?.models ?? []} />
           </div>
        </LabPanel>
        <LabPanel title="Fault Mapping by Model">
           <div className="h-[240px]">
             <ErrorByModelChart models={modelsData?.models ?? []} />
           </div>
        </LabPanel>
      </div>
    </div>
  )
}

