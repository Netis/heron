export interface ApiResponse<T> {
  code: number
  message: string
  data: T
}

// ---- Agent classification (Phase 5 — agent-era observer) ----
//
// Surfaces produced by the `ts_llm::agent_classifier` rule chain. Mirrors
// `ts_common::agent::ToolSurface` (serde `snake_case`).
export type ToolSurface = "function_call" | "mcp" | "cli" | "mixed" | "unknown"

// Mirrors `ts_common::agent::AgentTopology`.
export type AgentTopology = "single_agent" | "sub_agent" | "orchestrator"

// One suspicious tool flagged during turn rollup. Backend serializes the
// `tool_name` / `reason` pair into `agent_turns.suspicious_skills_json`.
export interface SuspiciousSkillRollup {
  tool_name: string
  reason: string
}

export interface LlmCallsPage {
  total: number
  items: LlmCallListItem[]
}

/**
 * Owning process of a captured connection. Only present on calls from an
 * attribution-capable source (eBPF); `null` for passive-tap sources (pcap /
 * cloud-probe), which see only the wire.
 */
export interface ProcessInfo {
  pid: number
  /** Kernel `comm` (≤15 chars), e.g. `python3`, `node`, `claude`. */
  comm: string
  /** Absolute executable path (`/proc/<pid>/exe`), or null when unresolved. */
  exe: string | null
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
  /** Agent classification — see Phase 5 plan. `false` for non-agent calls. */
  is_agent_request: boolean
  /** Tool surface detected by the classifier; null when not an agent call. */
  tool_surface: ToolSurface | null
  agent_topology: AgentTopology | null
  tool_call_count: number
  tool_names: string[]
  /** Owning process (eBPF attribution); null for passive-tap sources. */
  process: ProcessInfo | null
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

export interface ServicesData {
  services: ServiceRow[]
}

export interface ServiceRow {
  server_ip: string
  server_port: number
  models: string[]
  wire_apis: string[]
  request_paths: string[]
  call_count: number
  error_count: number
  stream_count: number
  total_input_tokens: number
  total_output_tokens: number
  ttft_avg_ms: number | null
  ttft_p95_ms: number | null
  e2e_avg_ms: number | null
  e2e_p95_ms: number | null
  first_seen_ms: number
  last_seen_ms: number
  /** Server software label — "vllm" / "sglang" / "ollama" /
   * "llamacpp" / "litellm" / "openai-compat" / "openai" / "anthropic"
   * / "gemini" / null (unknown). vLLM and SGLang both run on uvicorn
   * so they're currently bucketed as "openai-compat". */
  app: string | null
  /** Raw `Server` HTTP response header value, for the badge tooltip. */
  server_header: string | null
}

/** One node in the service-topology graph powering the Path view.
 * The synthetic clients aggregate uses `server_ip = "__clients__"` and
 * `server_port = 0` — the UI should treat those as a single
 * left-anchored super-node, not as a real endpoint. */
export interface TopologyNode {
  server_ip: string
  server_port: number
  app: string | null
  models: string[]
  call_count: number
}

/** One directed edge.
 *  - `kind: "proxy"` — pair-sweeper-confirmed hop, real service →
 *    real service.
 *  - `kind: "inferred"` — heuristic edge: caller_ip matches a known
 *    service's server_ip (typically LiteLLM forwarding to a real
 *    backend) but pair_sweeper hasn't paired the turn. Drawn as
 *    a dashed blue line so users can tell it apart from confirmed
 *    proxy hops.
 *  - `kind: "client"` — synthetic edge from `__clients__` into an
 *    entry-point service whose caller_ip doesn't resolve to any
 *    known service. */
export interface TopologyEdge {
  from_ip: string
  from_port: number
  to_ip: string
  to_port: number
  turn_count: number
  kind: "proxy" | "inferred" | "client"
}

export interface ServicesTopology {
  nodes: TopologyNode[]
  edges: TopologyEdge[]
}

/** One aggregate row per agent_kind over a time window. */
export interface AgentKindSummary {
  agent_kind: string
  turn_count: number
  total_input_tokens: number
  total_output_tokens: number
  avg_duration_ms: number | null
  last_seen_ms: number
}

export interface AgentSummaryData {
  summary: AgentKindSummary[]
}

/** One bucket-row of `(timestamp, agent_kind, count)` for the
 *  Overview activity chart. */
export interface AgentActivityPoint {
  timestamp_ms: number
  agent_kind: string
  turn_count: number
}

export interface AgentActivityData {
  points: AgentActivityPoint[]
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
  /** Set by the backend pair sweeper when this turn is one leg of a
   * llmproxy duplicate pair. `"proxy_in"` / `"mirror_primary"` legs are
   * the canonical (visible) members; `"proxy_out"` / `"mirror_secondary"`
   * are hidden by default and only returned when the API is asked with
   * `include_proxy_hops=true`. Absent on direct (non-proxied) turns. */
  proxy_role?: "proxy_in" | "proxy_out" | "mirror_primary" | "mirror_secondary"
  /** `turn_id` of the first matched peer leg (for backward compat).
   * For groups of >2 turns (haproxy 3-leg case), read
   * `proxy_peer_turn_ids` for the full list. */
  proxy_peer_turn_id?: string
  /** Every other member of this turn's proxy group, sorted lex. The
   * haproxy 3-leg case shows 2 peers here (br0 mirror + upstream hop). */
  proxy_peer_turn_ids?: string[]
  /** Distinct tool surfaces observed across the turn's calls. Empty for
   * turns with no classified surface (e.g. legacy non-agent traffic). */
  tool_surfaces: ToolSurface[]
  /** Sum of per-call `tool_call_count` across the turn. */
  tool_call_total: number
  /** Coarsest topology seen across calls (Orchestrator > SubAgent > SingleAgent). */
  agent_topology: AgentTopology | null
  /** Deduplicated suspicious tools flagged during rollup. */
  suspicious_skills: SuspiciousSkillRollup[]
}

// ---- Proxy view (multi-leg fold detail) ----

export interface ProxyViewMember {
  turn_id: string
  role: "proxy_in" | "proxy_out" | "mirror_primary" | "mirror_secondary" | string
  client_ip: string
  client_port: number | null
  server_ip: string
  server_port: number | null
  start_time: number
  end_time: number
  duration_ms: number
  ttft_ms: number | null
  e2e_latency_ms: number | null
  request_model: string | null
  wire_api: string
  request_path: string | null
  status_code: number | null
  request_headers: [string, string][]
  response_headers: [string, string][]
}

export interface HeaderValueByLeg {
  turn_id: string
  role: string
  value: string
}

export interface HeaderDiffEntry {
  name: string
  kind: "common" | "modified" | "per_leg"
  values: HeaderValueByLeg[]
}

export interface ModelRewrite {
  client_requested: string | null
  upstream_received: string | null
}

export interface LatencyBreakdown {
  client_observed_ms: number | null
  upstream_observed_ms: number | null
  proxy_overhead_ms: number | null
}

export interface ProxyViewResponse {
  group_id: string
  members: ProxyViewMember[]
  request_header_diff: HeaderDiffEntry[]
  response_header_diff: HeaderDiffEntry[]
  model_rewrite?: ModelRewrite
  latency_breakdown: LatencyBreakdown
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
  /** See AgentTurnListItem. */
  tool_surfaces: ToolSurface[]
  tool_call_total: number
  agent_topology: AgentTopology | null
  suspicious_skills: SuspiciousSkillRollup[]
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
  /** Agent classification — see LlmCallListItem. */
  is_agent_request: boolean
  tool_surface: ToolSurface | null
  agent_topology: AgentTopology | null
  tool_call_count: number
  tool_names: string[]
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
  /** Agent classification — see LlmCallListItem. */
  is_agent_request: boolean
  tool_surface: ToolSurface | null
  agent_topology: AgentTopology | null
  tool_call_count: number
  tool_names: string[]
  /** Owning process (eBPF attribution); null for passive-tap sources. */
  process: ProcessInfo | null
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
  /** See AgentTurnListItem. */
  tool_surfaces: ToolSurface[]
  tool_call_total: number
  agent_topology: AgentTopology | null
  suspicious_skills: SuspiciousSkillRollup[]
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
  | "ebpf"
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
// /api/internal-metrics/series
// ============================================================================

/** One point in an internal-metrics time series. `t` is unix epoch ms. */
export interface InternalSeriesPoint {
  t: number
  v: number
}

/** One series in the response — typically one per requested metric. */
export interface InternalSeriesEntry {
  name: string
  group: MetricGroup
  points: InternalSeriesPoint[]
}

export interface InternalMetricsSeriesResponse {
  /** Server wall-clock at response time, unix epoch ms. */
  ts: number
  series: InternalSeriesEntry[]
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
  /** Whether this binary can run an `ebpf` capture source (built with the
   * `ebpf` feature on Linux). Gates the Settings eBPF toggle. */
  ebpf_available: boolean
  /**
   * The live in-memory `AppConfig`. The narrow `AppConfigShape` typing is
   * used by Settings; the debug page still renders the whole thing as JSON
   * via `unknown` cast.
   */
  config: AppConfigShape
}

// Narrow typed view of the AppConfig fields the Settings page consumes.
// Everything else stays untyped — extend as needed.

export type CaptureSource =
  | {
      type: "pcap"
      interface: string
      bpf_filter: string | null
      snaplen: number
      source_id: string | null
    }
  | {
      type: "pcap-file"
      path: string
      realtime: boolean
      source_id: string | null
      // Loop/duration/rate replay (load-soak primitive). Mirror of the Rust
      // CaptureSourceConfig::PcapFile fields; serde defaults loop_count=1
      // (single pass) / loop_secs=0 / rate_pps=0 (disabled) so older configs
      // still parse.
      loop_count: number
      loop_secs: number
      rate_pps: number
    }
  | {
      type: "cloud-probe"
      endpoint: string
      recv_hwm: number
    }
  | {
      // On-host eBPF SSL-uprobe capture (Linux, experimental). Mirror of the
      // Rust CaptureSourceConfig::Ebpf fields. Empty ssl_libs = auto-discover
      // libssl; targets are static, symbol-stripped binaries (e.g. Bun).
      type: "ebpf"
      source_id: string | null
      ssl_libs: string[]
      targets: EbpfTarget[]
      pid_allowlist: number[]
      segment_size: number
    }

export interface EbpfTarget {
  binary: string
  flavor: string
  write_sig: string | null
  read_sig: string | null
  write_offset: number | null
  read_offset: number | null
}

export interface PcapDumpRetention {
  enabled: boolean
  check_interval_secs: number
  max_age_hours: number
  max_size_mb: number
}

export interface PcapDumpConfig {
  enabled: boolean
  dir: string
  compression: "none" | "snappy"
  retention?: PcapDumpRetention
}

export interface PipelineShape {
  name: string
  dispatcher_count?: number
  flow_shard_count?: number
  sources: CaptureSource[]
  pcap_dump?: PcapDumpConfig
}

export interface AppConfigShape {
  pipelines?: PipelineShape[]
  // Other AppConfig top-levels exist (storage, api, internal_metrics, …)
  // but Settings does not consume them.
  [k: string]: unknown
}

// ============================================================================
// /api/capture/interfaces
// ============================================================================

export interface CaptureInterface {
  name: string
  description: string | null
  addresses: string[]
  is_up: boolean
  is_running: boolean
  is_loopback: boolean
  is_wireless: boolean
}

export interface CaptureInterfacesResponse {
  interfaces: CaptureInterface[]
}
