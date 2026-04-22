use serde_json::Value;

use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{FinishReason, RequestInfo, ResponseInfo, RouteVerdict, WireApi};

/// Check for Bearer auth header (common to OpenAI variants).
fn has_bearer_auth(req: &HttpRequestData) -> bool {
    req.header("authorization")
        .map(|v| v.starts_with("Bearer "))
        .unwrap_or(false)
}

/// Header-level signals that rule OpenAI wire APIs out — the request is
/// unambiguously Anthropic. `anthropic-version` is the strongest (Anthropic
/// SDKs always set it); `Bearer sk-ant-*` is weaker because gateways that
/// re-sign keys can erase it, but when present it's a reliable negative.
fn is_anthropic_request(req: &HttpRequestData) -> bool {
    if req.header("anthropic-version").is_some() {
        return true;
    }
    req.header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.starts_with("sk-ant-"))
        .unwrap_or(false)
}

/// Shape signals common to both OpenAI variants: `model` string present and
/// no `input` field (that one belongs to the Responses API — Chat wire APIs
/// use it to disambiguate from OpenAI Responses, Responses uses its own rule
/// below which requires `input`).
fn has_openai_model_field(body: &Value) -> bool {
    body.get("model").and_then(|v| v.as_str()).is_some()
}

/// Wire-API implementation for OpenAI Chat Completions API.
pub struct OpenAiChatWireApi;

impl WireApi for OpenAiChatWireApi {
    fn name(&self) -> &'static str {
        super::OPENAI_CHAT
    }

    fn classify_route(&self, req: &HttpRequestData) -> RouteVerdict {
        if req.method != "POST" {
            return RouteVerdict::Reject;
        }
        // Header-level exclusion: Anthropic-shaped requests are not us.
        if is_anthropic_request(req) {
            return RouteVerdict::Reject;
        }
        let path = req.uri.split('?').next().unwrap_or(&req.uri);
        if path.ends_with("/v1/chat/completions") && has_bearer_auth(req) {
            return RouteVerdict::Accept;
        }
        RouteVerdict::Unknown
    }

    fn matches_shape(&self, _req: &HttpRequestData, body: &Value) -> bool {
        // Chat Completions requires `model` + `messages[]`. `input` present
        // means it's the Responses API, not us.
        if !has_openai_model_field(body) || body.get("input").is_some() {
            return false;
        }
        body.get("messages")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    }

    fn extract_request(&self, req: &HttpRequestData) -> RequestInfo {
        extract_from_request(req)
    }
    fn extract_response(&self, resp: &HttpResponseData) -> ResponseInfo {
        extract_from_response(resp)
    }
    fn extract_sse(&self, events: &[SseEventData]) -> ResponseInfo {
        extract_from_sse(events)
    }

    fn parse_output(&self, body: &Value) -> crate::model::ParsedOutput {
        use crate::model::{ParsedOutput, ParsedToolCall};
        let mut out = ParsedOutput::default();
        let Some(msg) = body
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
        else {
            return out;
        };
        if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
            if !c.is_empty() {
                out.message = Some(c.to_string());
            }
        }
        // Newer APIs also expose `reasoning_content` on reasoning models.
        if let Some(r) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
            if !r.is_empty() {
                out.reasoning = Some(r.to_string());
            }
        }
        if let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in arr {
                let id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let function = tc.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args_json = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                out.tool_calls.push(ParsedToolCall {
                    id,
                    name,
                    args_json,
                });
            }
        }
        out
    }

    fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
        use crate::model::{
            ParsedContentBlock, ParsedInput, ParsedMessage, ParsedRole, ParsedSampling,
            ParsedToolDef, ParsedToolResult,
        };
        let mut out = ParsedInput::default();

        // tools — OpenAI Chat: [{type:"function", function:{name, description, parameters}}]
        if let Some(arr) = body.get("tools").and_then(|v| v.as_array()) {
            for t in arr {
                let f = t.get("function");
                let name = f
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let description = f
                    .and_then(|v| v.get("description"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let input_schema_json = f
                    .and_then(|v| v.get("parameters"))
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                out.tools.push(ParsedToolDef {
                    name,
                    description,
                    input_schema_json,
                });
            }
        }

        // sampling
        out.sampling = ParsedSampling {
            temperature: body.get("temperature").and_then(|v| v.as_f64()),
            // Chat uses `max_tokens`; newer models accept `max_completion_tokens`.
            max_tokens: body
                .get("max_completion_tokens")
                .or_else(|| body.get("max_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            top_p: body.get("top_p").and_then(|v| v.as_f64()),
            top_k: None,
            stream: body.get("stream").and_then(|v| v.as_bool()),
            tool_choice: body.get("tool_choice").map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(v).unwrap_or_default(),
            }),
            stop: match body.get("stop") {
                Some(v) if v.is_string() => vec![v.as_str().unwrap().to_string()],
                Some(v) if v.is_array() => v
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => Vec::new(),
            },
            response_format: body
                .get("response_format")
                .map(|v| serde_json::to_string(v).unwrap_or_default()),
        };

        // messages
        let Some(messages) = body.get("messages").and_then(|v| v.as_array()) else {
            return out;
        };
        for msg in messages {
            let wire_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let role = match wire_role {
                "system" => ParsedRole::System,
                "user" => ParsedRole::User,
                "assistant" => ParsedRole::Assistant,
                "tool" => ParsedRole::Tool,
                _ => continue,
            };

            let mut blocks: Vec<ParsedContentBlock> = Vec::new();

            // content: string | array of content parts | null
            if let Some(s) = msg.get("content").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    blocks.push(ParsedContentBlock::Text {
                        text: s.to_string(),
                    });
                }
                if wire_role == "user" {
                    out.user_message = Some(s.to_string());
                }
            } else if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
                let mut user_text_buf = String::new();
                for part in arr {
                    match part.get("type").and_then(|v| v.as_str()) {
                        Some("text") | Some("input_text") => {
                            let text = part
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if wire_role == "user" {
                                if !user_text_buf.is_empty() {
                                    user_text_buf.push('\n');
                                }
                                user_text_buf.push_str(&text);
                            }
                            blocks.push(ParsedContentBlock::Text { text });
                        }
                        Some("image_url") => {
                            let mime = part
                                .get("image_url")
                                .and_then(|u| u.get("url"))
                                .and_then(|v| v.as_str())
                                .and_then(|url| {
                                    // data:image/png;base64,... → "image/png"
                                    url.strip_prefix("data:")
                                        .and_then(|rest| rest.split(';').next())
                                        .map(|s| s.to_string())
                                });
                            blocks.push(ParsedContentBlock::Image {
                                mime,
                                size_bytes: None,
                            });
                        }
                        _ => {
                            blocks.push(ParsedContentBlock::Unknown(part.clone()));
                        }
                    }
                }
                if wire_role == "user" && !user_text_buf.is_empty() {
                    out.user_message = Some(user_text_buf);
                }
            }

            // Assistant tool_calls → ToolUse blocks appended after any text content.
            if wire_role == "assistant" {
                if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let f = tc.get("function");
                        let name = f
                            .and_then(|v| v.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args_json = f
                            .and_then(|v| v.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        blocks.push(ParsedContentBlock::ToolUse {
                            id,
                            name,
                            args_json,
                        });
                    }
                }
            }

            // Tool-role message → ToolResult block.
            if wire_role == "tool" {
                let tool_use_id = msg
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content_str = match msg.get("content") {
                    Some(c) if c.is_string() => c.as_str().unwrap().to_string(),
                    Some(c) => serde_json::to_string(c).unwrap_or_default(),
                    None => String::new(),
                };
                // Legacy turn-joiner field.
                out.tool_results.push(ParsedToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content_str.clone(),
                    is_error: false,
                });
                // Replace any accumulated blocks — tool messages carry exactly one ToolResult.
                blocks = vec![ParsedContentBlock::ToolResult {
                    tool_use_id,
                    content: content_str,
                    is_error: false,
                }];
            }

            out.messages.push(ParsedMessage {
                role,
                content: blocks,
            });
        }

        out
    }
}

/// Wire-API implementation for OpenAI Responses API.
/// Shares extraction logic with Chat Completions.
pub struct OpenAiResponsesWireApi;

impl WireApi for OpenAiResponsesWireApi {
    fn name(&self) -> &'static str {
        super::OPENAI_RESPONSES
    }

    fn classify_route(&self, req: &HttpRequestData) -> RouteVerdict {
        if req.method != "POST" {
            return RouteVerdict::Reject;
        }
        if is_anthropic_request(req) {
            return RouteVerdict::Reject;
        }
        let path = req.uri.split('?').next().unwrap_or(&req.uri);
        if path.ends_with("/v1/responses") && has_bearer_auth(req) {
            return RouteVerdict::Accept;
        }
        RouteVerdict::Unknown
    }

    fn matches_shape(&self, _req: &HttpRequestData, body: &Value) -> bool {
        // Responses API discriminator: `input` present, `messages` absent.
        if !has_openai_model_field(body) {
            return false;
        }
        body.get("input").is_some() && body.get("messages").is_none()
    }

    fn extract_request(&self, req: &HttpRequestData) -> RequestInfo {
        extract_from_request(req)
    }
    fn extract_response(&self, resp: &HttpResponseData) -> ResponseInfo {
        extract_from_response(resp)
    }
    fn extract_sse(&self, events: &[SseEventData]) -> ResponseInfo {
        extract_from_sse(events)
    }

    fn parse_output(&self, body: &Value) -> crate::model::ParsedOutput {
        use crate::model::{ParsedOutput, ParsedToolCall};
        let mut out = ParsedOutput::default();
        let Some(items) = body.get("output").and_then(|v| v.as_array()) else {
            return out;
        };
        let mut reasoning_buf = String::new();
        let mut message_buf = String::new();
        for item in items {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("reasoning") => {
                    // `summary` is an array of { text } in current Responses schema.
                    if let Some(arr) = item.get("summary").and_then(|v| v.as_array()) {
                        for s in arr {
                            if let Some(t) = s.get("text").and_then(|v| v.as_str()) {
                                if !reasoning_buf.is_empty() {
                                    reasoning_buf.push('\n');
                                }
                                reasoning_buf.push_str(t);
                            }
                        }
                    }
                }
                Some("message") => {
                    if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
                        for part in arr {
                            if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                    if !message_buf.is_empty() {
                                        message_buf.push('\n');
                                    }
                                    message_buf.push_str(t);
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_json = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.tool_calls.push(ParsedToolCall {
                        id,
                        name,
                        args_json,
                    });
                }
                _ => {}
            }
        }
        if !reasoning_buf.is_empty() {
            out.reasoning = Some(reasoning_buf);
        }
        if !message_buf.is_empty() {
            out.message = Some(message_buf);
        }
        out
    }

    fn parse_input(&self, body: &Value) -> crate::model::ParsedInput {
        use crate::model::{
            ParsedContentBlock, ParsedInput, ParsedMessage, ParsedRole, ParsedSampling,
            ParsedToolDef, ParsedToolResult,
        };
        let mut out = ParsedInput::default();

        // instructions → system
        if let Some(s) = body.get("instructions").and_then(|v| v.as_str()) {
            out.system = Some(s.to_string());
        }

        // tools — Responses flavor: top-level {type:"function", name, description, parameters}.
        if let Some(arr) = body.get("tools").and_then(|v| v.as_array()) {
            for t in arr {
                let name = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let description = t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let input_schema_json = t
                    .get("parameters")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                out.tools.push(ParsedToolDef {
                    name,
                    description,
                    input_schema_json,
                });
            }
        }

        // sampling
        out.sampling = ParsedSampling {
            temperature: body.get("temperature").and_then(|v| v.as_f64()),
            // Responses uses `max_completion_tokens`; accept `max_tokens` too for robustness.
            max_tokens: body
                .get("max_completion_tokens")
                .or_else(|| body.get("max_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            top_p: body.get("top_p").and_then(|v| v.as_f64()),
            top_k: None,
            stream: body.get("stream").and_then(|v| v.as_bool()),
            tool_choice: body.get("tool_choice").map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(v).unwrap_or_default(),
            }),
            stop: match body.get("stop") {
                Some(v) if v.is_string() => vec![v.as_str().unwrap().to_string()],
                Some(v) if v.is_array() => v
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => Vec::new(),
            },
            response_format: body
                .get("response_format")
                .map(|v| serde_json::to_string(v).unwrap_or_default()),
        };

        // input: string | array of items
        if let Some(s) = body.get("input").and_then(|v| v.as_str()) {
            out.user_message = Some(s.to_string());
            out.messages.push(ParsedMessage {
                role: ParsedRole::User,
                content: vec![ParsedContentBlock::Text {
                    text: s.to_string(),
                }],
            });
            return out;
        }

        let Some(items) = body.get("input").and_then(|v| v.as_array()) else {
            return out;
        };

        for item in items {
            let type_field = item.get("type").and_then(|v| v.as_str());
            // Typeless items that carry a role are implicit messages (real OpenAI
            // Responses traffic often omits `type: "message"` when role+content
            // are already unambiguous).
            let is_message = type_field == Some("message")
                || (type_field.is_none() && item.get("role").is_some());
            if is_message {
                let wire_role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let role = match wire_role {
                    "system" => ParsedRole::System,
                    "user" => ParsedRole::User,
                    "assistant" => ParsedRole::Assistant,
                    _ => continue,
                };
                let mut blocks: Vec<ParsedContentBlock> = Vec::new();
                let mut user_text_buf = String::new();
                if let Some(s) = item.get("content").and_then(|v| v.as_str()) {
                    if wire_role == "user" {
                        out.user_message = Some(s.to_string());
                    }
                    if !s.is_empty() {
                        blocks.push(ParsedContentBlock::Text {
                            text: s.to_string(),
                        });
                    }
                } else if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
                    for part in arr {
                        match part.get("type").and_then(|v| v.as_str()) {
                            Some("input_text") | Some("text") | Some("output_text") => {
                                let text = part
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if wire_role == "user" {
                                    if !user_text_buf.is_empty() {
                                        user_text_buf.push('\n');
                                    }
                                    user_text_buf.push_str(&text);
                                }
                                blocks.push(ParsedContentBlock::Text { text });
                            }
                            Some("input_image") => {
                                let mime = part.get("image_url").and_then(|v| v.as_str()).and_then(
                                    |url| {
                                        url.strip_prefix("data:")
                                            .and_then(|rest| rest.split(';').next())
                                            .map(|s| s.to_string())
                                    },
                                );
                                blocks.push(ParsedContentBlock::Image {
                                    mime,
                                    size_bytes: None,
                                });
                            }
                            _ => blocks.push(ParsedContentBlock::Unknown(part.clone())),
                        }
                    }
                }
                if wire_role == "user" && !user_text_buf.is_empty() {
                    out.user_message = Some(user_text_buf);
                }
                out.messages.push(ParsedMessage {
                    role,
                    content: blocks,
                });
                continue;
            }
            match type_field {
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_json = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.messages.push(ParsedMessage {
                        role: ParsedRole::Assistant,
                        content: vec![ParsedContentBlock::ToolUse {
                            id,
                            name,
                            args_json,
                        }],
                    });
                }
                Some("function_call_output") => {
                    let tool_use_id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let content_str = match item.get("output") {
                        Some(c) if c.is_string() => c.as_str().unwrap().to_string(),
                        Some(c) => serde_json::to_string(c).unwrap_or_default(),
                        None => String::new(),
                    };
                    out.tool_results.push(ParsedToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: content_str.clone(),
                        is_error: false,
                    });
                    out.messages.push(ParsedMessage {
                        role: ParsedRole::Tool,
                        content: vec![ParsedContentBlock::ToolResult {
                            tool_use_id,
                            content: content_str,
                            is_error: false,
                        }],
                    });
                }
                _ => {
                    // Unknown *non-message* item — these are typically model-emitted
                    // (reasoning, file_search_call, computer_call, etc.) rather than
                    // user-authored, so attribute to Assistant rather than User.
                    out.messages.push(ParsedMessage {
                        role: ParsedRole::Assistant,
                        content: vec![ParsedContentBlock::Unknown(item.clone())],
                    });
                }
            }
        }

        out
    }
}

/// Extract request info from an OpenAI API request (both Chat and Responses).
pub fn extract_from_request(req: &HttpRequestData) -> RequestInfo {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Chat Completions defaults to non-streaming; explicit "stream": true opts in.
    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Tenant ID: prefix of the API key (first 10 chars) for tenant differentiation.
    // Not a cryptographic hash — sufficient for grouping, not for security.
    let tenant_id = req.header("authorization").map(|auth| {
        let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
        let prefix = if token.len() > 10 {
            &token[..10]
        } else {
            token
        };
        prefix.to_string()
    });

    RequestInfo {
        model,
        is_stream,
        tenant_id,
    }
}

/// Extract response info from a non-streaming OpenAI Chat Completions response.
pub fn extract_from_response(resp: &HttpResponseData) -> ResponseInfo {
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let body_str = std::str::from_utf8(&resp.body).ok().map(|s| s.to_string());

    let response_id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Chat Completions: choices[0].finish_reason
    let finish_reason = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(map_chat_finish_reason);

    let input_tokens = body
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let output_tokens = body
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let total_tokens = body
        .get("usage")
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let cache_read_input_tokens = body
        .get("usage")
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens: None,
        response_body: body_str,
        response_id,
    }
}

/// Extract response info from SSE events (OpenAI Responses API).
pub fn extract_from_sse(sse_events: &[SseEventData]) -> ResponseInfo {
    let mut model: Option<String> = None;
    let mut finish_reason: Option<FinishReason> = None;
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut total_tokens: Option<u32> = None;
    let mut cache_read_input_tokens: Option<u32> = None;
    let mut response_body: Option<String> = None;
    let mut response_id: Option<String> = None;
    // Codex (Responses API) streams output items via response.output_item.done
    // events; the terminal response.completed event carries an empty `output`
    // array. We accumulate finalized items here and graft them into the
    // response_body so downstream consumers (turn-terminal predicate, assistant
    // text extraction) see the full output.
    let mut output_items: Vec<Value> = Vec::new();

    for event in sse_events {
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);

        match event.event_type.as_str() {
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                // The response.completed event contains the full response object.
                if let Some(response) = data.get("response") {
                    if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                        response_id = Some(id.to_string());
                    }
                    if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                    if let Some(status) = response.get("status").and_then(|v| v.as_str()) {
                        finish_reason = Some(map_responses_status(status));
                    }
                    if let Some(usage) = response.get("usage") {
                        if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                            input_tokens = Some(it as u32);
                        }
                        if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                            output_tokens = Some(ot as u32);
                        }
                        if let Some(tt) = usage.get("total_tokens").and_then(|v| v.as_u64()) {
                            total_tokens = Some(tt as u32);
                        }
                        if let Some(cr) = usage
                            .get("prompt_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_input_tokens = Some(cr as u32);
                        }
                    }
                    // Graft accumulated output items into the response object
                    // when the wire copy is empty (codex behavior).
                    let mut response_obj = response.clone();
                    let needs_graft = response_obj
                        .get("output")
                        .and_then(|o| o.as_array())
                        .map(|a| a.is_empty())
                        .unwrap_or(true);
                    if needs_graft && !output_items.is_empty() {
                        if let Some(map) = response_obj.as_object_mut() {
                            map.insert("output".to_string(), Value::Array(output_items.clone()));
                        }
                    }
                    response_body = Some(response_obj.to_string());
                }
            }
            "response.created" => {
                // response.created also contains the response object with model.
                if let Some(response) = data.get("response") {
                    if model.is_none() {
                        if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                            model = Some(m.to_string());
                        }
                    }
                    if response_id.is_none() {
                        if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                            response_id = Some(id.to_string());
                        }
                    }
                }
            }
            // Chat Completions: chunks with no event_type may contain usage
            // in the final chunk (when stream_options: {"include_usage": true}).
            _ => {
                let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);

                // Chat Completions chunks: extract id from first chunk
                if response_id.is_none() {
                    if let Some(id) = data.get("id").and_then(|v| v.as_str()) {
                        response_id = Some(id.to_string());
                    }
                }

                // Chat Completions chunks: extract model
                if model.is_none() {
                    if let Some(m) = data.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                }

                // Chat Completions chunks: extract finish_reason from choices[0]
                if finish_reason.is_none() {
                    if let Some(fr) = data
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(|v| v.as_str())
                    {
                        finish_reason = Some(map_chat_finish_reason(fr));
                    }
                }

                // Chat Completions final chunk may include usage
                if input_tokens.is_none() {
                    if let Some(usage) = data.get("usage").filter(|u| u.is_object()) {
                        if let Some(it) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                            input_tokens = Some(it as u32);
                        }
                        if let Some(ot) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                            output_tokens = Some(ot as u32);
                        }
                        if let Some(tt) = usage.get("total_tokens").and_then(|v| v.as_u64()) {
                            total_tokens = Some(tt as u32);
                        }
                        if let Some(cr) = usage
                            .get("prompt_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_input_tokens = Some(cr as u32);
                        }
                    }
                }
            }
        }
    }

    // If no response.completed event was found (e.g. Chat Completions streaming),
    // try to assemble from chat-style SSE chunks.
    if response_body.is_none() {
        response_body = build_chat_response_body(sse_events);
    }

    ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens: None,
        response_body,
        response_id,
    }
}

/// Assemble a synthetic response body from OpenAI Chat Completions streaming chunks.
///
/// Chat Completions streaming sends chunks with `choices[0].delta.content` for text
/// and `choices[0].delta.tool_calls` for tool use. The last chunk has
/// `choices[0].finish_reason`. No event types — just `data: {...}` lines.
fn build_chat_response_body(sse_events: &[SseEventData]) -> Option<String> {
    use serde_json::json;

    let mut model: Option<String> = None;
    let mut content = String::new();
    let mut finish_reason: Option<String> = None;
    let mut tool_calls: std::collections::BTreeMap<u64, (String, String, String)> =
        std::collections::BTreeMap::new();
    let mut has_data = false;

    for event in sse_events {
        if event.data.trim() == "[DONE]" {
            continue;
        }
        let data: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);
        if !data.is_object() {
            continue;
        }
        has_data = true;

        if model.is_none() {
            if let Some(m) = data.get("model").and_then(|v| v.as_str()) {
                model = Some(m.to_string());
            }
        }

        if let Some(choice) = data.get("choices").and_then(|c| c.get(0)) {
            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                finish_reason = Some(fr.to_string());
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                    content.push_str(c);
                }
                // Accumulate tool calls by index
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                        let entry = tool_calls
                            .entry(idx)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            entry.0 = id.to_string();
                        }
                        if let Some(f) = tc.get("function") {
                            if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                                entry.1 = name.to_string();
                            }
                            if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
            }
        }

        // Also handle usage in the final chunk (stream_options: include_usage)
        // Skipped here since usage is already extracted in the main function.
    }

    if !has_data {
        return None;
    }

    // Build the synthetic message
    let mut message = json!({ "role": "assistant" });
    if !content.is_empty() {
        message["content"] = Value::String(content);
    }
    if !tool_calls.is_empty() {
        let tc_array: Vec<Value> = tool_calls
            .into_iter()
            .map(|(_, (id, name, args))| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args }
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(tc_array);
    }

    let result = json!({
        "model": model.unwrap_or_default(),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }]
    });

    Some(result.to_string())
}

/// Map OpenAI Chat Completions finish_reason to normalized FinishReason.
fn map_chat_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Complete,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::Cancelled,
        _ => FinishReason::Error,
    }
}

/// Map OpenAI Responses API status to normalized FinishReason.
fn map_responses_status(status: &str) -> FinishReason {
    match status {
        "completed" => FinishReason::Complete,
        "failed" => FinishReason::Error,
        "cancelled" => FinishReason::Cancelled,
        "incomplete" => FinishReason::Length,
        _ => FinishReason::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use ts_protocol::net::FlowKey;

    fn make_sse(event_type: &str, data: &str) -> SseEventData {
        let ip: IpAddr = IpAddr::from([127, 0, 0, 1]);
        SseEventData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            event_type: event_type.to_string(),
            data: data.to_string(),
            timestamp_us: 0,
        }
    }

    #[test]
    fn test_map_chat_finish_reason() {
        assert_eq!(map_chat_finish_reason("stop"), FinishReason::Complete);
        assert_eq!(map_chat_finish_reason("tool_calls"), FinishReason::ToolUse);
        assert_eq!(map_chat_finish_reason("length"), FinishReason::Length);
    }

    #[test]
    fn test_map_responses_status() {
        assert_eq!(map_responses_status("completed"), FinishReason::Complete);
        assert_eq!(map_responses_status("failed"), FinishReason::Error);
        assert_eq!(map_responses_status("incomplete"), FinishReason::Length);
    }

    #[test]
    fn test_build_chat_response_body_text() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":" world"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
            make_sse("", "[DONE]"),
        ];
        let body = build_chat_response_body(&events).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "gpt-4");
        assert_eq!(v["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_build_chat_response_body_tool_calls() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"SF\"}"}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ),
        ];
        let body = build_chat_response_body(&events).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        let tc = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["function"]["name"], "get_weather");
        // arguments must be a string, not a parsed object
        assert!(tc["function"]["arguments"].is_string());
        assert_eq!(tc["function"]["arguments"], r#"{"city":"SF"}"#);
    }

    #[test]
    fn test_build_chat_response_body_empty() {
        let events: Vec<SseEventData> = vec![];
        assert!(build_chat_response_body(&events).is_none());
    }

    #[test]
    fn test_extract_request_stream_default_false() {
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let body = serde_json::json!({"model": "gpt-4"});
        let req = HttpRequestData {
            flow_key: FlowKey::new(String::new(), ip, 1000, ip, 8080),
            client_addr: (ip, 1000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            version: 1,
            headers: vec![("authorization".to_string(), "Bearer sk-test".to_string())],
            body: bytes::Bytes::from(body.to_string()),
            timestamp_us: 0,
        };
        let info = extract_from_request(&req);
        // When "stream" is absent, Chat Completions defaults to false
        assert!(
            !info.is_stream,
            "stream should default to false for Chat Completions"
        );
    }

    #[test]
    fn test_extract_from_sse_chat_completions_finish_reason() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
            make_sse("", "[DONE]"),
        ];
        let info = extract_from_sse(&events);
        assert_eq!(
            info.finish_reason,
            Some(FinishReason::Complete),
            "finish_reason must be extracted from Chat Completions streaming chunks"
        );
    }

    #[test]
    fn test_extract_from_sse_chat_completions_finish_reason_tool_calls() {
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"f","arguments":""}}]}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ),
        ];
        let info = extract_from_sse(&events);
        assert_eq!(info.finish_reason, Some(FinishReason::ToolUse));
    }

    #[test]
    fn test_extract_from_sse_chat_completions_usage() {
        // Chat Completions final chunk with usage (stream_options: include_usage)
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            ),
        ];
        let info = extract_from_sse(&events);
        assert_eq!(info.input_tokens, Some(10));
        assert_eq!(info.output_tokens, Some(5));
        assert_eq!(info.total_tokens, Some(15));
    }

    #[test]
    fn test_extract_response_id_non_streaming() {
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "id": "chatcmpl-abc123",
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        });
        let resp = HttpResponseData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![],
            body: bytes::Bytes::from(body.to_string()),
            first_byte_timestamp_us: 0,
            complete_timestamp_us: 0,
        };
        let info = extract_from_response(&resp);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-abc123"));
    }

    #[test]
    fn test_extract_response_id_chat_completions_streaming() {
        let events = vec![
            make_sse(
                "",
                r#"{"id":"chatcmpl-stream1","model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            ),
            make_sse(
                "",
                r#"{"id":"chatcmpl-stream1","model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "",
                r#"{"id":"chatcmpl-stream1","model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ),
        ];
        let info = extract_from_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("chatcmpl-stream1"));
    }

    #[test]
    fn test_responses_api_grafts_output_items_into_empty_response_output() {
        // Real-codex behavior: response.completed.response.output is empty;
        // items arrive via response.output_item.done. Parser must accumulate
        // them and graft into the response_body so downstream consumers
        // (turn-terminal predicate, assistant-text extraction) can inspect them.
        let events = vec![
            make_sse(
                "response.created",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"in_progress"}}"#,
            ),
            make_sse(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","summary":[]}}"#,
            ),
            make_sse(
                "response.output_item.done",
                r#"{"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final."}]}}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
            ),
        ];
        let info = extract_from_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let out = v.get("output").and_then(|o| o.as_array()).unwrap();
        let types: Vec<&str> = out
            .iter()
            .map(|i| i.get("type").and_then(|t| t.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(types, vec!["reasoning", "message"]);
    }

    #[test]
    fn test_responses_api_preserves_nonempty_response_output() {
        // If response.completed already carries output (older/alternative codex
        // behavior), we must NOT overwrite it with the accumulated items.
        let events = vec![
            make_sse(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","summary":[]}}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x"}]}]}}"#,
            ),
        ];
        let info = extract_from_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let out = v.get("output").and_then(|o| o.as_array()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("type").and_then(|t| t.as_str()), Some("message"));
    }

    #[test]
    fn test_extract_response_id_responses_api_streaming() {
        let events = vec![
            make_sse(
                "response.created",
                r#"{"response":{"id":"resp_xyz","model":"gpt-4","status":"in_progress"}}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_xyz","model":"gpt-4","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
            ),
        ];
        let info = extract_from_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("resp_xyz"));
    }

    #[test]
    fn chat_parse_output_text() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat_output_text.json"
        ))
        .unwrap();
        let out = OpenAiChatWireApi.parse_output(&body);
        assert!(out
            .message
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        assert!(out.tool_calls.is_empty());
    }

    #[test]
    fn chat_parse_output_tool_calls() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat_output_tool_calls.json"
        ))
        .unwrap();
        let out = OpenAiChatWireApi.parse_output(&body);
        assert_eq!(out.tool_calls.len(), 1);
        assert!(out.tool_calls[0].id.starts_with("call_"));
    }

    #[test]
    fn chat_parse_input_tool_result() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat_input_with_tool_result.json"
        ))
        .unwrap();
        let out = OpenAiChatWireApi.parse_input(&body);
        assert_eq!(out.tool_results.len(), 1);
        assert!(out.tool_results[0].tool_use_id.starts_with("call_"));
    }

    #[test]
    fn responses_parse_output_message() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_responses_output_message.json"
        ))
        .unwrap();
        let out = OpenAiResponsesWireApi.parse_output(&body);
        assert!(out
            .message
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        assert!(out.tool_calls.is_empty());
    }

    #[test]
    fn responses_parse_output_function_call_with_reasoning() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_responses_output_function_call.json"
        ))
        .unwrap();
        let out = OpenAiResponsesWireApi.parse_output(&body);
        assert!(out.reasoning.is_some());
        assert_eq!(out.tool_calls.len(), 1);
    }

    #[test]
    fn responses_parse_input_with_function_call_output() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_responses_input_with_function_call_output.json"
        ))
        .unwrap();
        let out = OpenAiResponsesWireApi.parse_input(&body);
        assert_eq!(out.tool_results.len(), 1);
    }

    fn openai_chat_full_input() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_chat_input_full.json"
        ))
        .unwrap()
    }

    #[test]
    fn chat_parse_input_full_messages_roles() {
        use crate::model::{ParsedContentBlock, ParsedRole};
        let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
        let roles: Vec<ParsedRole> = out.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                ParsedRole::System,
                ParsedRole::User,
                ParsedRole::Assistant,
                ParsedRole::Tool,
                ParsedRole::Assistant,
            ]
        );
        // Assistant-with-tool_calls yields one ToolUse block (no text, since content was null).
        let assistant = &out.messages[2];
        assert_eq!(assistant.content.len(), 1);
        assert!(matches!(
            &assistant.content[0],
            ParsedContentBlock::ToolUse { name, args_json, .. }
                if name == "get_weather" && args_json == "{\"city\":\"SF\"}"
        ));
        // Tool role carries a ToolResult block.
        let tool = &out.messages[3];
        assert_eq!(tool.content.len(), 1);
        assert!(matches!(
            &tool.content[0],
            ParsedContentBlock::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call_1" && content == "72F sunny"
        ));
    }

    #[test]
    fn chat_parse_input_full_system_stays_in_messages() {
        // OpenAI Chat: top-level `system` field does not exist. System prompt is
        // the first `role=system` message inside `messages[]`.
        let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
        assert!(out.system.is_none());
        assert!(matches!(
            out.messages.first().map(|m| m.role),
            Some(crate::model::ParsedRole::System)
        ));
    }

    #[test]
    fn chat_parse_input_full_tools_extracted() {
        let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
        assert_eq!(out.tools.len(), 1);
        assert_eq!(out.tools[0].name, "get_weather");
        assert_eq!(
            out.tools[0].description.as_deref(),
            Some("Get weather for a city.")
        );
        let schema: serde_json::Value =
            serde_json::from_str(&out.tools[0].input_schema_json).unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn chat_parse_input_full_sampling_extracted() {
        let out = OpenAiChatWireApi.parse_input(&openai_chat_full_input());
        assert_eq!(out.sampling.temperature, Some(0.3));
        assert_eq!(out.sampling.max_tokens, Some(2048));
        assert_eq!(out.sampling.top_p, Some(1.0));
        assert_eq!(out.sampling.stream, Some(true));
        assert_eq!(out.sampling.stop, vec!["\n\n".to_string()]);
        assert_eq!(out.sampling.tool_choice.as_deref(), Some("auto"));
        assert_eq!(
            out.sampling.response_format.as_deref(),
            Some(r#"{"type":"json_object"}"#)
        );
    }

    fn openai_responses_full_input() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/openai_responses_input_full.json"
        ))
        .unwrap()
    }

    #[test]
    fn responses_parse_input_full_instructions_become_system() {
        let out = OpenAiResponsesWireApi.parse_input(&openai_responses_full_input());
        assert_eq!(out.system.as_deref(), Some("You are a code assistant."));
    }

    #[test]
    fn responses_parse_input_full_input_string_maps_to_user_message() {
        let body = serde_json::json!({
            "model": "gpt-5",
            "input": "hi"
        });
        let out = OpenAiResponsesWireApi.parse_input(&body);
        use crate::model::{ParsedContentBlock, ParsedRole};
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, ParsedRole::User);
        assert!(matches!(
            &out.messages[0].content[0],
            ParsedContentBlock::Text { text } if text == "hi"
        ));
    }

    #[test]
    fn responses_parse_input_full_items_roles_and_blocks() {
        use crate::model::{ParsedContentBlock, ParsedRole};
        let out = OpenAiResponsesWireApi.parse_input(&openai_responses_full_input());
        let roles: Vec<ParsedRole> = out.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![ParsedRole::User, ParsedRole::Assistant, ParsedRole::Tool,]
        );
        // assistant message carries one ToolUse block
        assert!(matches!(
            &out.messages[1].content[0],
            ParsedContentBlock::ToolUse { id, name, args_json }
                if id == "call_abc" && name == "run_shell" && args_json == "{\"cmd\":\"ls\"}"
        ));
        // tool message carries one ToolResult block
        assert!(matches!(
            &out.messages[2].content[0],
            ParsedContentBlock::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call_abc" && content == "a.txt\nb.txt"
        ));
    }

    #[test]
    fn responses_parse_input_full_tools_and_sampling() {
        let out = OpenAiResponsesWireApi.parse_input(&openai_responses_full_input());
        assert_eq!(out.tools.len(), 1);
        assert_eq!(out.tools[0].name, "run_shell");
        assert_eq!(out.sampling.temperature, Some(0.2));
        assert_eq!(out.sampling.max_tokens, Some(4096));
        assert_eq!(out.sampling.top_p, Some(1.0));
        assert_eq!(out.sampling.stream, Some(true));
        assert_eq!(out.sampling.tool_choice.as_deref(), Some("auto"));
    }

    #[test]
    fn responses_parse_input_typeless_message_item_is_handled() {
        // Real OpenAI Responses shape (see tests/fixtures/openai-responses/request-image-input.json):
        // items without `type: "message"` but carrying `role` + `content`.
        use crate::model::{ParsedContentBlock, ParsedRole};
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/openai-responses/request-image-input.json"
        ))
        .unwrap();
        let out = OpenAiResponsesWireApi.parse_input(&body);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, ParsedRole::User);
        // Two parts: input_text + input_image
        assert_eq!(out.messages[0].content.len(), 2);
        assert!(matches!(
            &out.messages[0].content[0],
            ParsedContentBlock::Text { .. }
        ));
        assert!(matches!(
            &out.messages[0].content[1],
            ParsedContentBlock::Image { .. }
        ));
    }

    #[test]
    fn responses_parse_input_function_call_output_non_string() {
        // Non-string `output` on function_call_output — e.g., a JSON object —
        // should be JSON-stringified via serde_json::to_string (not panic).
        let body = serde_json::json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_x",
                    "output": {"status": "ok", "data": [1, 2, 3]}
                }
            ]
        });
        let out = OpenAiResponsesWireApi.parse_input(&body);
        use crate::model::{ParsedContentBlock, ParsedRole};
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, ParsedRole::Tool);
        let result_content = match &out.messages[0].content[0] {
            ParsedContentBlock::ToolResult { content, .. } => content.clone(),
            other => panic!("expected ToolResult, got {:?}", other),
        };
        // Parse back — verifies JSON round-trip.
        let parsed: serde_json::Value = serde_json::from_str(&result_content).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["data"][0], 1);
        // Legacy tool_results should also receive the stringified form.
        assert_eq!(out.tool_results.len(), 1);
        assert_eq!(out.tool_results[0].tool_use_id, "call_x");
    }

    #[test]
    fn responses_parse_input_system_role_message() {
        // System role on a message item — exercises the "system" => ParsedRole::System arm.
        use crate::model::{ParsedContentBlock, ParsedRole};
        let body = serde_json::json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "text", "text": "be brief"}]
                }
            ]
        });
        let out = OpenAiResponsesWireApi.parse_input(&body);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, ParsedRole::System);
        assert!(matches!(
            &out.messages[0].content[0],
            ParsedContentBlock::Text { text } if text == "be brief"
        ));
        // System prompt did NOT go into top-level `out.system` — that only happens
        // for top-level `instructions`, not for `role=system` inside input items.
        assert!(out.system.is_none());
    }
}
