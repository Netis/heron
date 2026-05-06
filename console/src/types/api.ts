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
  ttft_ms: number | null
  e2e_latency_ms: number | null
  input_tokens: number | null
  output_tokens: number | null
  /**
   * True when input/output tokens came from the fallback tiktoken estimator
   * instead of a wire-side `usage` block. Surfaces a `~` prefix on tokens
   * columns. Optional because old API builds may not include it.
   */
  tokens_estimated?: boolean
  client_ip: string
  server_ip: string
  server_port: number
  request_path: string
}

// Metrics types

export interface MetricsSummary {
  call_count: number
  error_count: number
  error_4xx_count: number
  error_429_count: number
  error_5xx_count: number
  total_input_tokens: number
  total_output_tokens: number
  ttft_avg: number | null
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
  call_count: number
  error_count: number
  error_4xx_count: number
  error_429_count: number
  error_5xx_count: number
  total_input_tokens: number
  total_output_tokens: number
  ttft_avg: number | null
  ttft_p95: number | null
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
  client_ip: string
  server_ip: string
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
  source_id: string
  session_id: string
  wire_api: string
  agent_kind: string
  client_ip: string
  server_ip: string
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
  ttft_ms: number | null
  e2e_latency_ms: number | null
  input_tokens: number | null
  output_tokens: number | null
  /** See LlmCallListItem.tokens_estimated. */
  tokens_estimated?: boolean
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
  source_id: string
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
  /** See LlmCallListItem.tokens_estimated. */
  tokens_estimated?: boolean
  ttft_ms: number | null
  e2e_latency_ms: number | null
  response_id: string | null
  client_ip: string
  client_port: number
  server_ip: string
  server_port: number
  request_body: string | null
  response_body: string | null
  request_headers: string | null
  response_headers: string | null
}

// HTTP exchange types — /api/http-exchanges

export interface HttpExchangeListItem {
  id: string
  source_id: string
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
  source_id: string
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

// Agent session types — /api/agent-sessions

export interface SessionListItem {
  source_id: string
  session_id: string
  agent_kind: string
  /** ms since epoch — MAX(end_time) across windowed turns, the sort key */
  last_turn_at_in_window: number
  first_turn_at: number
  last_turn_at: number
  turn_count: number
  call_count: number
  total_input_tokens: number
  total_output_tokens: number
  total_cache_read_input_tokens: number
  total_cache_creation_input_tokens: number
  total_cost_usd: number | null
  first_user_input_preview: string | null
  first_user_call_id: string | null
}

export interface SessionsPage {
  items: SessionListItem[]
  /** Opaque cursor. null when the current page is the last one. */
  next_cursor: string | null
}

export interface SessionDetail {
  source_id: string
  session_id: string
  agent_kind: string
  first_turn_at: number
  last_turn_at: number
  turn_count: number
  call_count: number
  total_input_tokens: number
  total_output_tokens: number
  total_cache_read_input_tokens: number
  total_cache_creation_input_tokens: number
  total_cost_usd: number | null
  first_user_input_preview: string | null
  first_user_call_id: string | null
}

export interface SessionTurnItem {
  turn_id: string
  source_id: string
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
  /** Full text. Frontend truncates for collapsed preview (~120 chars). */
  user_input: string | null
  /** Full text. Null when the turn ended without a final answer. */
  final_answer: string | null
}

export interface SessionTurnsPage {
  items: SessionTurnItem[]
  next_cursor: string | null
}

// ============================================================================
// /api/internal-metrics
// ============================================================================

export type MetricGroup =
  | "capture"
  | "protocol"
  | "llm"
  | "turn"
  | "metrics"
  | "storage"

export type MetricKind = "counter" | "gauge"

export interface MetricRecord {
  name: string
  group: MetricGroup
  kind: MetricKind
  value: number
  capacity?: number
}

export interface PipelineMetricsSnapshot {
  name: string
  metrics: MetricRecord[]
}

export interface InternalMetricsResponse {
  ts: number
  pipelines: PipelineMetricsSnapshot[]
  global: { metrics: MetricRecord[] }
}

// ============================================================================
// /api/runtime-config
// ============================================================================

export interface RuntimeConfigResponse {
  /** Unix epoch ms when AppConfig::load returned in the running process. */
  loaded_at_ms: number
  /** Absolute path of the config file the running process read at startup. */
  config_path: string
  /** Binary version (env!("CARGO_PKG_VERSION") at compile time). */
  version: string
  /**
   * The live in-memory `AppConfig`. Shape mirrors the Rust struct but is kept
   * opaque on the TS side — the page renders it as JSON and there is no other
   * consumer that needs typed access.
   */
  config: unknown
}
