export interface ApiResponse<T> {
  code: number
  message: string
  data: T
}

export interface LlmCallsPage {
  total: number
  items: LlmCallListItem[]
}

export interface LlmCallListItem {
  id: string
  request_time: number
  wire_api: string
  model: string
  status_code: number | null
  is_stream: boolean
  finish_reason: string | null
  ttfb_ms: number | null
  e2e_latency_ms: number | null
  input_tokens: number | null
  output_tokens: number | null
}

// Metrics types

export interface MetricsSummary {
  request_count: number
  error_count: number
  error_4xx_count: number
  error_429_count: number
  error_5xx_count: number
  total_input_tokens: number
  total_output_tokens: number
  ttfb_avg: number | null
  e2e_avg: number | null
  tpot_avg: number | null
}

export interface TimeseriesData {
  timestamps: number[]
  series: TimeseriesSeries[]
}

export interface TimeseriesSeries {
  name: string
  group: string | null
  values: (number | null)[]
}

export interface ModelsData {
  models: MetricsModelRow[]
}

export interface MetricsModelRow {
  wire_api: string
  model: string
  request_count: number
  error_count: number
  error_4xx_count: number
  error_429_count: number
  error_5xx_count: number
  total_input_tokens: number
  total_output_tokens: number
  ttfb_avg: number | null
  ttfb_p95: number | null
  e2e_avg: number | null
  e2e_p95: number | null
  tpot_avg: number | null
}

// Agent turn list + detail types

export interface AgentTurnsPage {
  total: number
  items: AgentTurnListItem[]
}

export interface AgentTurnListItem {
  turn_id: string
  session_id: string
  start_time: number
  end_time: number
  duration_ms: number
  wire_api: string
  agent_kind: string
  primary_model: string | null
  models_used: string[]
  call_count: number
  total_input_tokens: number
  total_output_tokens: number
  status: string
  final_finish_reason: string | null
  user_input_preview: string | null
  final_answer_preview: string | null
}

export interface AgentTurnDetail {
  turn_id: string
  session_id: string
  tenant_id: string | null
  wire_api: string
  agent_kind: string
  start_time: number
  end_time: number
  duration_ms: number
  call_count: number
  models_used: string[]
  subagents_used: string[]
  total_input_tokens: number
  total_output_tokens: number
  total_cached_input_tokens: number
  total_cost_usd: number | null
  status: string
  final_finish_reason: string | null
  user_call_id: string | null
  user_input: string | null
  final_call_id: string | null
  final_answer: string | null
  call_ids: string[]
  metadata: unknown
}

export interface AgentTurnCallItem {
  id: string
  sequence: number
  request_time: number
  response_time: number | null
  complete_time: number | null
  wire_api: string
  model: string
  status_code: number | null
  is_stream: boolean
  finish_reason: string | null
  ttfb_ms: number | null
  e2e_latency_ms: number | null
  input_tokens: number | null
  output_tokens: number | null
  request_path: string
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  /** Raw request body. Frontend parses per-wire_api for preview + detail. */
  request_body: string | null
  response_body: string | null
  /** JSON-encoded `[[header_name, header_value], ...]`. */
  request_headers: string | null
  response_headers: string | null
}

// LLM call detail — raw payload. Frontend parses per-wire_api via
// @/lib/wire-apis/<provider>/index.ts.
export interface LlmCallDetail {
  id: string
  request_time: number
  response_time: number | null
  complete_time: number | null
  wire_api: string
  model: string
  api_type: string
  is_stream: boolean
  request_path: string
  status_code: number | null
  finish_reason: string | null
  input_tokens: number | null
  output_tokens: number | null
  total_tokens: number | null
  ttfb_ms: number | null
  e2e_latency_ms: number | null
  response_id: string | null
  tenant_id: string | null
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  request_body: string | null
  response_body: string | null
  request_headers: string | null
  response_headers: string | null
  /** Populated via LEFT JOIN on agent_turns — null when the call is not part of a turn. */
  agent_kind: string | null
}

// HTTP exchange types — /api/http-exchanges

export interface HttpExchangeListItem {
  id: string
  stream_id: string
  request_time: number
  method: string
  uri: string
  client_ip: string
  server_ip: string
  server_port: number
  status: number | null
  is_sse: boolean
  duration_ms: number | null
}

export interface HttpExchangesPage {
  total: number
  items: HttpExchangeListItem[]
}

export interface HttpExchangeDetail {
  id: string
  stream_id: string
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  method: string
  uri: string
  /// JSON-encoded `[[header_name, header_value], ...]`
  request_headers: string
  request_body: string | null
  status: number | null
  response_headers: string
  response_body: string | null
  is_sse: boolean
  /** Number of SSE events observed. 0 for non-SSE. */
  sse_event_count: number
  /** Sum of SSE `data:` payload bytes. Frame overhead excluded. 0 for non-SSE. */
  sse_data_bytes: number
  request_time: number
  response_first_byte_time: number | null
  response_complete_time: number | null
}
