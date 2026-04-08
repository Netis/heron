# LLM Module Design

## Overview

The `ts-llm` crate detects the LLM Provider from an HTTP exchange, extracts LLM-specific semantics using provider-specific extractors, and tracks agent loop lifecycle. It consumes `HttpExchange` from `ts-protocol` and outputs `LlmRequest` + `LlmLoop`.

## Provider Registry Pattern

Inspired by [CLIProxyAPIPlus](https://github.com/router-for-me/CLIProxyAPI) translator architecture: global registry with per-provider detection + extraction.

```rust
/// Identifies the API format
pub enum ProviderFormat {
    OpenAI,             // /v1/chat/completions
    OpenAIResponses,    // /v1/responses
    Anthropic,          // /v1/messages
    Azure,              // /openai/deployments/*/chat/completions
    Gemini,             // /v1beta/models/*/generateContent
    Generic,            // OpenAI-compatible fallback (vLLM, Ollama, etc.)
}

/// Determines if an HTTP request matches a specific provider
pub trait ProviderDetector: Send + Sync {
    fn detect(&self, req: &HttpRequest) -> Option<ProviderFormat>;
}

/// Extracts LLM semantics from an HTTP exchange
pub trait ProviderExtractor: Send + Sync {
    fn extract(&self, exchange: &HttpExchange) -> Result<LlmRequest>;
}

/// Registry: maps formats to detectors + extractors
pub struct ProviderRegistry {
    detectors: Vec<(Box<dyn ProviderDetector>, ProviderFormat)>,
    extractors: HashMap<ProviderFormat, Box<dyn ProviderExtractor>>,
}

impl ProviderRegistry {
    pub fn register(&mut self, format: ProviderFormat,
                    detector: impl ProviderDetector,
                    extractor: impl ProviderExtractor);

    /// Auto-detect provider and return extractor
    pub fn detect(&self, req: &HttpRequest) -> Option<(ProviderFormat, &dyn ProviderExtractor)>;
}
```

## Provider Differences

What each extractor needs to handle:

| | OpenAI | Anthropic | Azure | Gemini | Generic |
|---|---|---|---|---|---|
| **Path** | `/v1/chat/completions` | `/v1/messages` | `/openai/deployments/*/...` | `/v1beta/models/*/generateContent` | configurable |
| **Auth header** | `Authorization: Bearer sk-...` | `x-api-key: sk-ant-...` | `api-key: ...` | `x-goog-api-key: ...` | varies |
| **SSE event type** | not used | `content_block_delta`, `message_delta`, etc. | not used | not used | usually not used |
| **Token delta in SSE** | `choices[0].delta.content` | `delta.text` | `choices[0].delta.content` | `candidates[0].content.parts[0].text` | usually OpenAI-compatible |
| **Usage field** | `usage.{prompt,completion}_tokens` | `usage.{input,output}_tokens` | same as OpenAI + extra | `usageMetadata.{prompt,candidates}TokenCount` | varies |
| **Finish marker** | `finish_reason` | `stop_reason` | `finish_reason` | `finishReason` | varies |
| **Tool use signal** | `finish_reason: "tool_calls"` | `stop_reason: "tool_use"` | same as OpenAI | function call in response | usually same as OpenAI |

## Output Model

See [schema.md](schema.md) for the full data schema. The Rust types:

```rust
pub struct LlmRequest {
    pub id: String,                     // UUID v7
    pub provider: ProviderFormat,
    pub model: String,
    pub api_type: ApiType,              // Chat / Embedding / Image / ...
    pub tenant_id: Option<String>,      // Hashed API key prefix
    pub loop_id: Option<String>,        // Set by LoopTracker
    pub loop_index: Option<u32>,
    pub connection_id: Option<String>,
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
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub client_ip: IpAddr,
    pub client_port: u16,
    pub server_port: u16,
    pub server_node: Option<String>,
}

pub enum FinishReason {
    Complete,   // OpenAI: "stop", Anthropic: "end_turn"
    Length,     // Max tokens reached
    ToolUse,    // Agent loop continues
    Error,
    Cancelled,
}
```

## Agent Loop Tracking

After each `LlmRequest` is produced, it passes through the `LoopTracker` which manages agent loop lifecycle. See [schema.md](schema.md) for the `llm_loops` data model and state machine.

### Provider Independence

`LoopTracker` only inspects the normalized `FinishReason` — it has no knowledge of provider-specific formats. Each provider's extractor is responsible for mapping its native signals to `FinishReason::ToolUse` (loop continues) or other variants (loop ends). This means different providers can have completely different tool-use conventions, and LoopTracker works the same way.

### Graceful Degradation

Not all requests participate in agent loops. Embedding requests, image generation, or providers that don't support tool use will never produce `FinishReason::ToolUse`. In these cases, LoopTracker creates a single-request loop (request_count=1) that immediately completes — effectively a no-op. No special handling needed.

```rust
pub struct LoopTracker {
    /// Active loops: connection_id → ActiveLoop
    active_loops: HashMap<String, ActiveLoop>,
    /// Timeout for inactive loops
    timeout: Duration,
}

impl LoopTracker {
    /// Called for each completed LlmRequest
    fn on_request_complete(&mut self, record: &mut LlmRequest) -> LoopEvent {
        // 1. Find active loop for this connection
        // 2. None → create new loop, set record.loop_id and loop_index=0
        // 3. Some → update aggregates, set record.loop_id and loop_index
        // 4. Check finish_reason:
        //    - ToolUse → LoopEvent::Continued
        //    - Other   → close loop, LoopEvent::Completed
    }

    /// Periodic timeout check
    fn check_timeouts(&mut self) -> Vec<LlmLoop> { ... }
}

enum LoopEvent {
    Started { loop_id: String },
    Continued { loop_id: String },
    Completed { loop: LlmLoop },
}
```

## Adding a New Provider

1. Create `providers/new_provider.rs`
2. Implement `ProviderDetector` (URL/header matching rules)
3. Implement `ProviderExtractor` (JSON parsing for request/response/SSE)
4. Register in `providers/mod.rs` → `register_all()`

No changes to `ts-protocol`, `ts-storage`, or `ts-api`.

## File Structure

```
ts-llm/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── format.rs           # ProviderFormat enum
    ├── detector.rs         # ProviderDetector trait
    ├── extractor.rs        # ProviderExtractor trait
    ├── registry.rs         # ProviderRegistry
    ├── model.rs            # LlmRequest, LlmLoop, FinishReason and supporting types
    ├── loop_tracker.rs     # LoopTracker — agent loop lifecycle management
    └── providers/
        ├── mod.rs           # register_all()
        ├── openai.rs
        ├── openai_responses.rs
        ├── anthropic.rs
        ├── azure.rs
        ├── gemini.rs
        └── generic.rs       # OpenAI-compatible fallback
```
