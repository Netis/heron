import type { ApiResponse } from "@/types/api"

const BASE_URL = ""

export class ApiError extends Error {
  code: number

  constructor(code: number, message: string) {
    super(message)
    this.name = "ApiError"
    this.code = code
  }
}

export async function apiFetch<T>(
  path: string,
  params?: Record<string, string | number | boolean | undefined>,
): Promise<T> {
  const url = new URL(path, window.location.origin)
  if (params) {
    for (const [key, value] of Object.entries(params)) {
      if (value !== undefined && value !== "") {
        url.searchParams.set(key, String(value))
      }
    }
  }

  const res = await fetch(`${BASE_URL}${url.pathname}${url.search}`)
  if (!res.ok) {
    const body = await res.json().catch(() => ({ code: res.status, message: res.statusText }))
    throw new ApiError(body.code ?? res.status, body.message ?? res.statusText)
  }

  const json: ApiResponse<T> = await res.json()
  if (json.code !== 0) {
    throw new ApiError(json.code, json.message)
  }
  return json.data
}

export interface DownloadResult {
  /** Items skipped during a batch export (unsupported wire / unavailable body). */
  skipped: number
  total: number
  written: number
}

/**
 * Download a raw (non-`ApiResponse`) endpoint as a file via a blob + anchor
 * click — used for the trajectory export endpoints, which serve NDJSON with a
 * `Content-Disposition` attachment rather than the JSON envelope. Returns the
 * `X-Export-*` counters when present (batch export) so callers can surface how
 * many turns were skipped.
 */
export async function downloadFile(path: string, fallbackName: string): Promise<DownloadResult> {
  const res = await fetch(path)
  if (!res.ok) {
    const body = await res.json().catch(() => ({ code: res.status, message: res.statusText }))
    throw new ApiError(body.code ?? res.status, body.message ?? res.statusText)
  }

  const num = (h: string) => Number(res.headers.get(h) ?? "0") || 0
  const result: DownloadResult = {
    skipped: num("x-export-skipped"),
    total: num("x-export-total"),
    written: num("x-export-written"),
  }

  const disposition = res.headers.get("content-disposition") ?? ""
  const match = disposition.match(/filename="?([^"]+)"?/)
  const filename = match?.[1] ?? fallbackName

  const blob = await res.blob()
  const objectUrl = URL.createObjectURL(blob)
  const a = document.createElement("a")
  a.href = objectUrl
  a.download = filename
  document.body.appendChild(a)
  a.click()
  a.remove()
  URL.revokeObjectURL(objectUrl)

  return result
}
