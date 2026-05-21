import { BrowserRouter, Routes, Route } from "react-router"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { AppLayout } from "@/components/layout/app-layout"
import { OverviewPage } from "@/pages/overview"
import { PerformancePage } from "@/pages/performance"
import { TrafficPage } from "@/pages/traffic"
import { ErrorsPage } from "@/pages/errors"
import { ModelsPage } from "@/pages/models"
import { ServicesPage } from "@/pages/services"
import { LlmCallsPage } from "@/pages/llm-calls"
import { AgentSessionsPage } from "@/pages/agent-sessions"
import { AgentSessionDetailPage } from "@/pages/agent-session-detail"
import { AgentTurnsPage } from "@/pages/agent-turns"
import { HttpExchangesPage } from "@/pages/http-exchanges"
import { SettingsPage } from "@/pages/settings"
import { PipelineHealthPage } from "@/pages/debug-pipeline-health"
import { RuntimeConfigPage } from "@/pages/debug-runtime-config"
import { DebugIndexPage } from "@/pages/debug-index"

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
})

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <Routes>
          <Route element={<AppLayout />}>
            <Route path="/" element={<OverviewPage />} />
            <Route path="/performance" element={<PerformancePage />} />
            <Route path="/traffic" element={<TrafficPage />} />
            <Route path="/errors" element={<ErrorsPage />} />
            <Route path="/models" element={<ModelsPage />} />
            <Route path="/services" element={<ServicesPage />} />
            <Route path="/agent-sessions" element={<AgentSessionsPage />} />
            <Route path="/agent-sessions/:source_id/:session_id" element={<AgentSessionDetailPage />} />
            <Route path="/agent-turns" element={<AgentTurnsPage />} />
            <Route path="/llm-calls" element={<LlmCallsPage />} />
            <Route path="/http-exchanges" element={<HttpExchangesPage />} />
            <Route path="/settings" element={<SettingsPage />} />
          </Route>
          <Route path="/debug" element={<DebugIndexPage />} />
          <Route path="/debug/pipeline-health" element={<PipelineHealthPage />} />
          <Route path="/debug/config" element={<RuntimeConfigPage />} />
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  )
}
