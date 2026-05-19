import { useMutation } from "@tanstack/react-query"
import { ApiError } from "@/lib/api"
import type { ApiResponse, CaptureSource } from "@/types/api"

export interface UpdateSourcesRequest {
  pipeline_name: string
  sources: CaptureSource[]
}

export interface UpdateSourcesResponse {
  /** How long the server will wait before re-execing itself. */
  restart_in_ms: number
}

async function putUpdate(body: UpdateSourcesRequest): Promise<UpdateSourcesResponse> {
  const res = await fetch("/api/capture/sources", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  })
  if (!res.ok) {
    const parsed = await res.json().catch(() => ({ code: res.status, message: res.statusText }))
    throw new ApiError(parsed.code ?? res.status, parsed.message ?? res.statusText)
  }
  const json: ApiResponse<UpdateSourcesResponse> = await res.json()
  if (json.code !== 0) throw new ApiError(json.code, json.message)
  return json.data
}

/**
 * Mutation to PUT new capture sources to the server. On success the server
 * schedules a self-restart after `restart_in_ms` — callers should show a
 * "restarting…" overlay and poll `/api/health` to detect when the new
 * process is up.
 */
export function useUpdateSources() {
  return useMutation({
    mutationFn: putUpdate,
  })
}
