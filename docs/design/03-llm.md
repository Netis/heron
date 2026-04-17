# LLM Module Design

## Overview

The `ts-llm` crate detects the LLM Provider from HTTP requests/responses and extracts LLM-specific semantics using provider-specific extractors. It consumes `HttpRequest`, `HttpResponse`, and `SseEvent` from `ts-protocol` and outputs `LlmCall`.

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
    fn extract(&self, request: &HttpRequest, response: &HttpResponse) -> Result<LlmCall>;
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
pub struct LlmCall {
    pub id: String,                     // UUID v7
    pub provider: ProviderFormat,
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
    pub ttfb_ms: Option<f64>,
    pub e2e_latency_ms: Option<f64>,
    pub client_ip: IpAddr,
    pub client_port: u16,
    pub server_ip: IpAddr,
    pub server_port: u16,
}

pub enum FinishReason {
    Complete,   // OpenAI: "stop", Anthropic: "end_turn"
    Length,     // Max tokens reached
    ToolUse,    // Agent will make another call
    Error,
    Cancelled,
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
    ├── model.rs            # LlmCall, FinishReason and supporting types
    └── providers/
        ├── mod.rs           # register_all()
        ├── openai.rs
        ├── openai_responses.rs
        ├── anthropic.rs
        ├── azure.rs
        ├── gemini.rs
        └── generic.rs       # OpenAI-compatible fallback
```
