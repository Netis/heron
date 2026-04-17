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
  params?: Record<string, string | number | undefined>,
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
