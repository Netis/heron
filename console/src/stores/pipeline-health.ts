import { create } from "zustand"

type PipelineHealthState = {
  /** Polling interval in ms. `null` = paused. */
  intervalMs: number | null
  /** Selected pipeline name. `null` = use the first one returned. */
  selectedPipeline: string | null
  /** All-Metrics table filter chip ("all" | group name). */
  tableGroupFilter: string
  /** All-Metrics table "only ⚠" toggle. */
  tableOnlyWarn: boolean

  setIntervalMs: (ms: number | null) => void
  setSelectedPipeline: (name: string | null) => void
  setTableGroupFilter: (chip: string) => void
  setTableOnlyWarn: (on: boolean) => void
}

export const usePipelineHealthStore = create<PipelineHealthState>((set) => ({
  intervalMs: 2000,
  selectedPipeline: null,
  tableGroupFilter: "all",
  tableOnlyWarn: false,
  setIntervalMs: (intervalMs) => set({ intervalMs }),
  setSelectedPipeline: (selectedPipeline) => set({ selectedPipeline }),
  setTableGroupFilter: (tableGroupFilter) => set({ tableGroupFilter }),
  setTableOnlyWarn: (tableOnlyWarn) => set({ tableOnlyWarn }),
}))
