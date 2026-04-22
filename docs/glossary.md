# Glossary

Terms and concepts that show up in the TokenScope UI, configuration, and stored data. Pipeline-internal mechanics live in the per-module design docs under [design/](design/).

## Performance Metrics

Shown on dashboards and in the detail views. Percentiles (`p50`/`p95`/`p99`) are per-row estimates over that row's slice. Averages remain exact across any group of rows because each average field is stored as a `sum` + `count` pair.

| Term | Definition | Unit |
|------|-----------|------|
| **TTFT** | Time To First Token — `response_time − request_time`. First response byte after request sent. For streaming this is the user-perceived "it started typing" latency; for non-streaming it equals E2E. Dominated by server-side prefill. | ms |
| **E2E latency** | End-to-End latency — `complete_time − request_time`. Full wall time from request issued to response finished. | ms |
| **TPOT** | Time Per Output Token — `(complete_time − response_time) / output_tokens`. **Streaming only.** Steady-state per-token decode cost once generation has started. | ms/tok |
| **QPS** | Queries per second. `request_count / window_seconds`. Derived at query time. | req/s |
| **Throughput** | Output tokens per second. `total_output_tokens / window_seconds`. Derived. | tok/s |
| **Concurrency** | Overlapping in-flight requests in a window. Per-row average = `concurrency_sum / concurrency_sample_count`; peak = `concurrency_max`. | count |
| **Cache hit ratio** | `total_cache_read_input_tokens / total_input_tokens`. Signals prompt-cache effectiveness. | ratio |
| **Error rate / 429 rate** | `error_count / request_count` (all HTTP ≥ 400) and `error_429_count / request_count` (rate-limited). | ratio |

## Data Entities

Four tables (`http_exchanges`, `llm_calls`, `agent_turns`, `llm_metrics`) — what the console and SQL queries read.

| Entity | Table | Scope |
|--------|-------|-------|
| **HttpExchange** | `http_exchanges` | One paired `(request, response)` at the HTTP layer. Wire-API-agnostic — non-LLM traffic lands here too. Carries SSE counters (`sse_event_count`, `sse_data_bytes`) but not reconstructed SSE content. |
| **LlmCall** | `llm_calls` | One LLM API call — model, tokens, TTFT/E2E, status, full bodies, `response_id`. The wire-API-shaped semantic record and the core detail view. |
| **AgentTurn** | `agent_turns` | One user→agent interaction cycle: 1..N `LlmCall`s grouped by `(session_id)` and bounded by a main-agent terminal call. Only produced for traffic from a recognised agent client (`claude-cli`, `codex-cli`, …). |
| **LlmMetric** | `llm_metrics` | One pre-aggregated row per `(window_start, granularity, wire_api, model, server_ip)`. Time-series fact table for dashboards. |

Relationship: each `LlmCall` derives from one `HttpExchange`; a single `AgentTurn` references its calls by `call_ids`; `LlmMetric` is a time-window rollup of `LlmCall`.

## Wire API & Agent Classification

Two orthogonal filter axes in the UI. `wire_api` describes the *server* API shape; `agent_kind` describes the *client* making the calls.

| Field | Meaning | Example values |
|------|---------|---------------|
| **wire_api** | The on-wire HTTP shape — method + path + body schema — of one LLM API. This is the HTTP API *shape*, not the vendor. Stored verbatim on `LlmCall.wire_api`. | `anthropic`, `openai-chat`, `openai-responses` |
| **Vendor** | The organisation serving a wire API (OpenAI, Anthropic, Azure, vLLM, Ollama). Multiple vendors can speak the same wire API — Azure / vLLM / Ollama all speak `openai-chat`. Not yet a first-class field; inferred from hostname / key prefix / route when needed. | — |
| **agent_kind** | Short stable identifier of an agent *client* recognised from its request headers. Persisted to `AgentTurn.agent_kind`. Header-explicit-only: unrecognised clients produce `LlmCall`s but never `AgentTurn`s. | `claude-cli`, `codex-cli` |

## Identifiers You Encounter in Data

| Field | Appears on | Purpose |
|-------|-----------|---------|
| `session_id` | `AgentTurn` | Agent-client session. Extracted from client-specific headers — Claude uses `X-Claude-Code-Session-Id`; Codex uses body/header `session_id`. |
| `turn_id` | `AgentTurn` | UUIDv7 assigned when the turn finalises. Monotonic by emission time. |
| `tenant_id` | `LlmCall` | Hashed API-key prefix. Anonymous tenant dimension for cross-tenant analytics. |
| `response_id` | `LlmCall` | Provider's own response/message ID (`chatcmpl-…`, `msg_…`). For cross-referencing with provider logs. |
| `id` (entity PK) | `HttpExchange`, `LlmCall` | UUIDv7 — time-ordered. |

## Wire-Level Concepts

| Term | Meaning |
|------|---------|
| **SSE** | Server-Sent Events. HTTP streaming format (`text/event-stream`) used by all three implemented wire APIs for streaming responses. Each event is `event: <type>\ndata: <json>\n\n`. Reconstructed SSE content drives semantic extraction but is not persisted — only per-exchange counts (`sse_event_count`, `sse_data_bytes`). |
| **is_stream** | Whether the request opted into SSE. Determines whether TTFT diverges from E2E and whether TPOT is defined. |
| **FinishReason** | Normalised terminal signal across vendors. Values: `complete` / `length` / `tool_use` / `error` / `cancelled`. Source field differs per wire API — OpenAI `finish_reason: "stop"` and Anthropic `stop_reason: "end_turn"` both map to `complete`. |
| **Prompt cache** | Provider-side re-use of previously-seen prompt prefixes. Observed as `cache_read_input_tokens` (hit; billed cheaper) and `cache_creation_input_tokens` (miss — tokens written to cache for future reuse; Anthropic-only). |
| **Prefill / decode split** | Inference-cluster concept. Prefill = consuming the prompt (parallel, one-shot) — dominates TTFT. Decode = generating output tokens (sequential) — dominates TPOT. Operators tune the split from TokenScope data. |

## Metric Windows & Retention

| Term | Meaning |
|------|---------|
| **Granularity** | Metric window size. Four run in parallel and are selectable on the dashboard: `10s` (realtime), `1m` (recent trends), `5m` (mid-term), `1h` (historical). |
| **`*` (star) dimension** | String literal `*` in a `llm_metrics` dimension column means "all values". Lets the table carry both drill-down rows and global rollups in the same schema. |
| **Retention** | Per-table / per-granularity TTL, configured under `[storage.retention]` in `tokenscope.toml`. Opt-in; `0` or absent means "never expire". |

## Domain / Positioning

| Term | Meaning |
|------|---------|
| **BPC** | Behavioral Packet Capture — the pre-LLM discipline of inferring business behavior from enterprise network traffic. TokenScope extends BPC into the LLM era, where the payload is already structured intent / plan / outcome. See [mission.md](mission.md). |
| **Provider-side** | Deployment posture: TokenScope reads **post-TLS plaintext HTTP** at the LLM provider, not at the client. No SDK instrumentation, no man-in-the-middle on the client. |
| **cloud-probe** | [Netis/cloud-probe](https://github.com/Netis/cloud-probe) — a ZMQ-forwarding packet probe. One of TokenScope's two capture ingresses (the other is local libpcap). Not a requirement. |

## See also

- [docs/design/03-llm.md](design/03-llm.md) — wire-API list and extraction
- [docs/design/04-turn.md](design/04-turn.md) — what an `AgentTurn` records
- [docs/design/07-schema.md](design/07-schema.md) — storage schema (column-level reference)
