import { BrowserRouter, Routes, Route } from "react-router"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { AppLayout } from "@/components/layout/app-layout"
import { OverviewPage } from "@/pages/overview"
import { PerformancePage } from "@/pages/performance"
import { TrafficPage } from "@/pages/traffic"
import { ErrorsPage } from "@/pages/errors"
import { ModelsPage } from "@/pages/models"
import { RequestsPage } from "@/pages/requests"
import { SourcesPage } from "@/pages/sources"
import { TurnsPage } from "@/pages/turns"

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
            <Route path="/requests" element={<RequestsPage />} />
            <Route path="/turns" element={<TurnsPage />} />
            <Route path="/sources" element={<SourcesPage />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  )
}
