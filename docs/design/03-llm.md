# LLM Module Design

## Overview

The `ts-llm` crate detects the LLM wire API from HTTP requests/responses and extracts LLM-specific semantics using per-wire-API extractors. It consumes `HttpRequest`, `HttpResponse`, and `SseEvent` from `ts-protocol` and outputs `LlmCall`.

### Terminology

- **Wire API** — the on-wire HTTP shape (method + path + body schema) of a single LLM API. Examples: `openai-chat` (Chat Completions), `openai-responses` (Responses API), `anthropic` (Anthropic Messages API for now). This is what `ts-llm` detects and parses.
- **Vendor** — the organization that serves a given wire API (OpenAI, Anthropic, Azure, Google, self-hosted vLLM). Multiple vendors can speak the same wire API (Azure OpenAI, vLLM, Ollama all speak `openai-chat`). Not yet a first-class field in TokenScope; if/when we need it it will come from hostname / key prefix / route prefix.

`LlmCall.wire_api` is persisted verbatim to storage as the compound `<vendor>-<api>` form (e.g. `openai-chat`) so operator filter UIs stay self-descriptive until an explicit vendor dimension lands.

## Wire-API Registry Pattern

Inspired by [CLIProxyAPIPlus](https://github.com/router-for-me/CLIProxyAPI) translator architecture: global registry with per-wire-API detection + extraction.

```rust
pub trait WireApi: Send + Sync {
    /// Stable identifier (e.g. "openai-chat"). Persisted verbatim to storage
    /// as `LlmCall.wire_api`.
    fn name(&self) -> &'static str;

    /// Pass 1: inspect method + URI + headers only. Runs on every HTTP
    /// request so it must be cheap. Returns RouteVerdict::{Accept, Reject,
    /// Unknown}.
    fn classify_route(&self, req: &HttpRequestData) -> RouteVerdict;

    /// Pass 2: inspect parsed JSON body. Called only when classify_route
    /// returned Unknown for every wire API and the body parses as JSON.
    fn matches_shape(&self, req: &HttpRequestData, body: &Value) -> bool;

    /// Extraction methods, called after a wire API has won detection.
    fn extract_request(&self, req: &HttpRequestData) -> RequestInfo;
    fn extract_response(&self, resp: &HttpResponseData) -> ResponseInfo;
    fn extract_sse(&self, events: &[SseEventData]) -> ResponseInfo;
}

pub struct WireApiRegistry { /* Vec<Box<dyn WireApi>> */ }

impl WireApiRegistry {
    pub fn detect(&self, req: &HttpRequestData) -> Option<&dyn WireApi>;
    pub fn find_by_name(&self, name: &str) -> Option<&dyn WireApi>;
}
```

Detection is two-pass:
1. `classify_route` on every wire API. An `Accept` short-circuits and wins. `Reject` candidates drop out.
2. If nobody accepted and at least one returned `Unknown`, parse the request body once and call `matches_shape` on the remaining candidates in registry order; the first match wins.

## Wire-API Differences

What each extractor needs to handle:

| | openai-chat | anthropic | azure-openai (future) | gemini (future) | generic (future) |
|---|---|---|---|---|---|
| **Path** | `/v1/chat/completions` | `/v1/messages` | `/openai/deployments/*/...` | `/v1beta/models/*/generateContent` | configurable |
| **Auth header** | `Authorization: Bearer sk-...` | `x-api-key: sk-ant-...` | `api-key: ...` | `x-goog-api-key: ...` | varies |
| **SSE event type** | not used | `content_block_delta`, `message_delta`, etc. | not used | not used | usually not used |
| **Token delta in SSE** | `choices[0].delta.content` | `delta.text` | `choices[0].delta.content` | `candidates[0].content.parts[0].text` | usually openai-chat-compatible |
| **Usage field** | `usage.{prompt,completion}_tokens` | `usage.{input,output}_tokens` | same as openai-chat + extra | `usageMetadata.{prompt,candidates}TokenCount` | varies |
| **Finish marker** | `finish_reason` | `stop_reason` | `finish_reason` | `finishReason` | varies |
| **Tool use signal** | `finish_reason: "tool_calls"` | `stop_reason: "tool_use"` | same as openai-chat | function call in response | usually same as openai-chat |

`openai-responses` (OpenAI Responses API) is also implemented; it shares the OpenAI auth and extraction shape but uses `/v1/responses` with an `input`-based body schema.

## Output Model

See [07-schema.md](07-schema.md) for the full data schema. The Rust types:

```rust
pub struct LlmCall {
    pub id: String,                     // UUID v7
    /// Stable wire-API identifier (e.g. "openai-chat", "anthropic",
    /// "openai-responses"). This is the HTTP API shape, not the vendor.
    pub wire_api: &'static str,
    pub model: String,
    pub api_type: ApiType,              // Chat / Embedding / Image / ...
    pub tenant_id: Option<String>,      // Hashed API key prefix
    pub request_time: Timestamp,
    pub response_time: Option<Timestamp>,
    pub complete_time: Option<Timestamp>,
    pub request_path: String,
    pub is_stream: bool,
    pub request_body: Option<String>,
    pub status_code: Option<u16>,
    pub finish_reason: Option<FinishReason>,
    pub response_body: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub ttft_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub client_ip: IpAddr,
    pub client_port: u16,
    pub server_ip: IpAddr,
    pub server_port: u16,
}

pub enum FinishReason {
    Complete,   // openai-chat: "stop", anthropic: "end_turn"
    Length,     // Max tokens reached
    ToolUse,    // Agent will make another call
    Error,
    Cancelled,
}
```

## Adding a New Wire API

1. Create `wire_apis/new_api.rs`
2. Implement `WireApi` (URL/header match rules + JSON parsing for request/response/SSE)
3. Add the constant in `wire_apis/mod.rs` (e.g. `GEMINI_V1BETA`)
4. Register it in `wire_apis::build_default_wire_api_registry()`

No changes to `ts-protocol`, `ts-storage`, or `ts-api`.

## File Structure

```
ts-llm/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── model.rs              # LlmCall, FinishReason, WireApi trait, RouteVerdict
    ├── profile.rs            # AgentProfile trait + AgentProfileRegistry
    ├── processor.rs          # LlmProcessor: ProtocolEvent → LlmEvent
    ├── stage.rs              # spawn_llm_stage (shards + fan-out)
    ├── wire_api_registry.rs  # WireApiRegistry
    ├── wire_apis/
    │   ├── mod.rs            # wire-API name constants + build_default_wire_api_registry()
    │   ├── anthropic.rs      # AnthropicMessagesWireApi
    │   └── openai.rs         # OpenAiChatWireApi + OpenAiResponsesWireApi
    └── profiles/
        ├── mod.rs
        ├── claude_cli.rs
        └── codex_cli.rs
```
