//! OpenAI Responses wire API (`POST /v1/responses`).
//!
//! Strict to the Responses shape — top-level `status`, `usage.input_tokens`,
//! `usage.output_tokens`, `usage.input_tokens_details.cached_tokens`. No
//! `.or_else()` fallback to the Chat shape: the registry has already
//! selected us, so ambiguity would be a bug.
//!
//! SSE handling keeps the codex-compat grafting: when `response.completed`
//! arrives with an empty `output` array but prior `response.output_item.done`
//! events supplied items, we splice them into the body so downstream
//! consumers always see the full output.

use serde_json::Value;

use h_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{RequestInfo, ResponseInfo, RouteVerdict, WireApi};
use crate::parsed_json::ParsedJson;

use super::shared::{has_bearer_auth, is_anthropic_request};

pub struct OpenAiResponsesWireApi;

impl WireApi for OpenAiResponsesWireApi {
    fn name(&self) -> &'static str {
        crate::wire_apis::OPENAI_RESPONSES
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

    fn matches_shape(&self, _req: &HttpRequestData, req_body: &ParsedJson) -> bool {
        let Some(req_body) = req_body.get() else {
            return false;
        };
        // Responses discriminator: `model` + `input` present, `messages` absent.
        req_body.get("model").and_then(|v| v.as_str()).is_some()
            && req_body.get("input").is_some()
            && req_body.get("messages").is_none()
    }

    fn extract_request(&self, req: &HttpRequestData, req_body: &ParsedJson) -> RequestInfo {
        extract_request(req, req_body)
    }
    fn extract_response(&self, resp: &HttpResponseData, resp_body: &ParsedJson) -> ResponseInfo {
        extract_response(resp, resp_body)
    }
    fn extract_sse(&self, events: &[SseEventData]) -> (ResponseInfo, ParsedJson) {
        extract_sse(events)
    }

    fn is_terminal(&self, finish_reason: &str) -> bool {
        matches!(
            finish_reason,
            "completed" | "incomplete" | "failed" | "cancelled"
        )
    }

    fn is_tool_use(&self, _finish_reason: &str) -> bool {
        // Responses API surfaces tool use via output items, not finish_reason.
        // Keep predicate false; tracker should rely on output-item inspection.
        false
    }
}

fn extract_request(_req: &HttpRequestData, req_body: &ParsedJson) -> RequestInfo {
    let body = req_body.get();
    RequestInfo {
        model: body
            .and_then(|b| b.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        is_stream: body
            .and_then(|b| b.get("stream"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn extract_response(resp: &HttpResponseData, resp_body: &ParsedJson) -> ResponseInfo {
    let parsed = resp_body.get();
    let body_str = std::str::from_utf8(&resp.body).ok().map(|s| s.to_string());

    let response_id = parsed
        .and_then(|b| b.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model = parsed
        .and_then(|b| b.get("model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let finish_reason = parsed
        .and_then(|b| b.get("status"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let usage = parsed.and_then(|b| b.get("usage"));
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let total_tokens = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let cache_read_input_tokens = usage
        .and_then(|u| u.get("input_tokens_details"))
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

fn extract_sse(events: &[SseEventData]) -> (ResponseInfo, ParsedJson) {
    let mut model: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut total_tokens: Option<u32> = None;
    let mut cache_read_input_tokens: Option<u32> = None;
    // Build the assembled response as a `Value`; only serialize once at the
    // end for storage. The same `Value` is handed to `body` so downstream
    // readers get it without a String->Value round-trip.
    let mut response_value: Option<Value> = None;
    let mut response_id: Option<String> = None;
    // Codex streams output items via response.output_item.done; the terminal
    // response.completed often carries an empty `output` array. Accumulate
    // here and graft into the final body below.
    let mut output_items: Vec<Value> = Vec::new();

    for event in events {
        match event.event_type.as_str() {
            "response.created" => {
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(response) = data.get("response") {
                    if response_id.is_none() {
                        if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                            response_id = Some(id.to_string());
                        }
                    }
                    if model.is_none() {
                        if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                            model = Some(m.to_string());
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(item) = data.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                let data: Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(response) = data.get("response") {
                    if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                        response_id = Some(id.to_string());
                    }
                    if let Some(m) = response.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                    if let Some(status) = response.get("status").and_then(|v| v.as_str()) {
                        finish_reason = Some(status.to_string());
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
                            .get("input_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_input_tokens = Some(cr as u32);
                        }
                    }

                    // Graft accumulated items into the response object when
                    // the wire copy is missing or empty (codex behavior);
                    // otherwise preserve the wire copy verbatim.
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
                    response_value = Some(response_obj);
                }
            }
            // Anything else (including untyped Chat-style chunks) is not ours.
            _ => {}
        }
    }

    let response_body = response_value.as_ref().map(Value::to_string);
    let response_cache = ParsedJson::from_value(response_value);

    let info = ResponseInfo {
        model,
        finish_reason,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens: None,
        response_body,
        response_id,
    };
    (info, response_cache)
}

/// Decide whether an OpenAI Responses body represents a terminal turn
/// (agent done, no more tool roundtrips pending).
///
/// Logic: scan `response.output[]`. Any item whose `type` ends with `_call`
/// (e.g., `function_call`, `custom_tool_call`, `local_shell_call`, MCP
/// variants) means the agent will execute a tool and re-POST → not terminal.
/// `message` items count as the final answer; `reasoning` is ignored.
/// Return true iff at least one `message` is present and no `*_call` is.
///
/// Used by both `CodexCliProfile` and `GenericProfile` (openai-responses
/// branch) — the OpenAI Responses protocol always sets `status: "completed"`
/// on successful API calls regardless of whether the agent continues, so the
/// wire-api `finish_reason` is unreliable for turn-boundary purposes. This
/// helper is the authoritative override.
///
/// Re-exported via `crate::wire_apis::openai::body_has_terminal_message_only`
/// (the `responses` submodule itself is private). Profiles should use the
/// re-exported path.
pub fn body_has_terminal_message_only(response_body: Option<&str>) -> bool {
    let Some(body) = response_body else {
        return false;
    };
    let Ok(resp) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    body_has_terminal_message_only_value(&resp)
}

/// `Value`-input sibling of `body_has_terminal_message_only`. Used by
/// `AgentProfile::is_turn_terminal` overrides on the parse-once hot path.
pub fn body_has_terminal_message_only_value(resp: &Value) -> bool {
    let Some(output) = resp.get("output").and_then(|o| o.as_array()) else {
        return false;
    };
    let mut has_message = false;
    for item in output {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("message") => has_message = true,
            Some("reasoning") => {}
            Some(t) if t.ends_with("_call") => return false,
            _ => {}
        }
    }
    has_message
}

// ────────────────────────── Body shape parsers ─────────────────────────────
//
// Helpers that walk OpenAI Responses request `input[]` / response
// `output[]` arrays for the pieces agent profiles need. The Responses
// shape is item-oriented (each item has its own `type`), so most helpers
// take `&[Value]` rather than the wrapper `&Value` — agent profiles
// extract the array once via `v.get("input")` / `v.get("output")` and
// pass the slice in.

use crate::wire_apis::AssistantSig;

/// Extract `tools[].name`. Responses tools are FLAT — each entry is
/// `{type:"function", name, parameters, ...}` (no inner `function`
/// wrapper like OpenAI Chat uses). Same `Some(empty) | None` semantics as
/// the Anthropic / OpenAI-Chat counterparts: absent → empty list,
/// wrong shape → `None`.
pub fn tool_names(req: &Value) -> Option<Vec<String>> {
    match req.get("tools") {
        None => Some(Vec::new()),
        Some(Value::Array(arr)) => Some(
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect(),
        ),
        Some(_) => None,
    }
}

/// First system-prompt text in a Responses request. Two placements are
/// possible and must both be checked:
///   1. Top-level `instructions` field (codex-style, mostly a Codex path).
///   2. A `{type:"message", role:"system"|"developer"}` item in `input[]`
///      (the standard non-codex Responses path used by SDKs that fold the
///      system prompt into the conversation).
/// `instructions` wins if present (it's the explicit channel); otherwise
/// fall back to the `input[]` walk.
pub fn first_system_text(req: &Value) -> Option<String> {
    if let Some(s) = req.get("instructions").and_then(|x| x.as_str()) {
        if !s.trim().is_empty() {
            return Some(s.to_string());
        }
    }
    let items = req.get("input")?.as_array()?;
    for it in items {
        if it.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let role = it.get("role").and_then(|v| v.as_str());
        if role != Some("system") && role != Some("developer") {
            continue;
        }
        if let Some(t) = message_text(it) {
            return Some(t);
        }
    }
    None
}

/// Concatenate visible text from a single Responses `message` item's
/// content. Accepts the legacy string shorthand and the canonical
/// `[{"type":"output_text","text":...}]` (or `"input_text"`/`"text"`)
/// array form.
pub fn message_text(item: &Value) -> Option<String> {
    let c = item.get("content")?;
    match c {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    let t = b.get("type").and_then(|v| v.as_str())?;
                    if t == "text" || t == "input_text" || t == "output_text" {
                        b.get("text").and_then(|v| v.as_str()).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Text of the **first** `type:"message", role:"user"` item. If that item
/// has no extractable text, return `None` instead of falling through to
/// later user messages (which would be turn 2+ — not a stable anchor).
pub fn first_user_text(items: &[Value]) -> Option<String> {
    let first_user = items.iter().find(|it| {
        it.get("type").and_then(|v| v.as_str()) == Some("message")
            && it.get("role").and_then(|v| v.as_str()) == Some("user")
    })?;
    message_text(first_user)
}

/// Scan a single generation's items for sig. Prefers the earliest
/// `function_call.call_id`; falls back to the earliest assistant
/// `message`'s text. The `function_call` precedence matches Anthropic's
/// `tool_use` precedence in `wire_apis::anthropic::first_assistant_sig_*`.
///
/// Caller must have already bounded `items` to a single generation — this
/// helper does **not** do any turn / generation boundary detection.
fn scan_generation_for_sig(items: &[Value]) -> Option<AssistantSig> {
    let mut tool_id: Option<String> = None;
    let mut text: Option<String> = None;
    for it in items {
        let t = it.get("type").and_then(|v| v.as_str());
        if tool_id.is_none() && t == Some("function_call") {
            if let Some(id) = it.get("call_id").and_then(|v| v.as_str()) {
                tool_id = Some(id.to_string());
            }
        }
        if text.is_none()
            && t == Some("message")
            && it.get("role").and_then(|v| v.as_str()) == Some("assistant")
        {
            if let Some(t) = message_text(it) {
                text = Some(t);
            }
        }
        if tool_id.is_some() && text.is_some() {
            break;
        }
    }
    if let Some(id) = tool_id {
        Some(AssistantSig::ToolId(id))
    } else {
        text.map(AssistantSig::Text)
    }
}

/// Sig of the **first assistant generation** in a request `input[]` (multi-
/// turn conversation history). Bounding has two stages — both needed
/// because Responses uses flat sibling items (`reasoning` / `message` /
/// `function_call` / `function_call_output`) rather than nested
/// message-with-blocks like Anthropic / OpenAI Chat:
///
///   1. **Skip the entire leading user run.** Some clients (Codex CLI,
///      Gemini-style scaffolding ports) split turn 1's user side into two
///      consecutive `role:user` items — a synthetic environment_context
///      followed by the real prompt. A naive "first user → next user" window
///      would collapse those two into an empty window. Skip past the whole
///      run of consecutive leading user messages instead.
///   2. **Stop at the first `function_call_output`.** That marks the boundary
///      between generation 1 and generation 2 of the same turn. Without this
///      bound, a generation-1 text-only response followed by a generation-2
///      `function_call` (within the same turn) would yield `ToolId(fc_e2)`
///      from input but `Text(msg_e1)` from response — splitting the session
///      id between call 1 and call 2.
pub fn first_assistant_sig_from_input(items: &[Value]) -> Option<AssistantSig> {
    let is_user_message = |it: &Value| {
        it.get("type").and_then(|v| v.as_str()) == Some("message")
            && it.get("role").and_then(|v| v.as_str()) == Some("user")
    };
    let is_fco =
        |it: &Value| it.get("type").and_then(|v| v.as_str()) == Some("function_call_output");

    // (1) Skip past the whole run of leading user messages. `start` lands on
    //     the first non-user item — the first assistant generation begins here.
    let start = match items.iter().position(is_user_message) {
        None => 0,
        Some(idx) => {
            let mut i = idx;
            while i < items.len() && is_user_message(&items[i]) {
                i += 1;
            }
            i
        }
    };
    // Turn 1 ends at the next `role:user` message (start of turn 2) or array
    // end. Used only to scope the generation boundary search below.
    let turn_end = items[start..]
        .iter()
        .position(is_user_message)
        .map(|i| start + i)
        .unwrap_or(items.len());
    // (2) Generation 1 ends at the first `function_call_output` within turn 1,
    //     or at turn 1's end if there isn't one.
    let generation_end = items[start..turn_end]
        .iter()
        .position(is_fco)
        .map(|i| start + i)
        .unwrap_or(turn_end);

    scan_generation_for_sig(&items[start..generation_end])
}

pub fn first_assistant_sig_from_response(body: &str) -> Option<AssistantSig> {
    let resp: Value = serde_json::from_str(body).ok()?;
    first_assistant_sig_from_response_value(&resp)
}

/// Sig of the assistant generation in a response `output[]`. A response is
/// the model's single inference product, so `output[]` IS one generation —
/// no turn / generation boundaries to detect, just scan it directly. (Splits
/// from `first_assistant_sig_from_input` deliberately: input's bounding
/// logic is meaningless for output and only adds coupling risk if future
/// API surfaces ever introduce input-shaped items in responses.)
pub fn first_assistant_sig_from_response_value(resp: &Value) -> Option<AssistantSig> {
    let output = resp.get("output")?.as_array()?;
    scan_generation_for_sig(output)
}

/// Extract the latest user-message text in `input[]`. Walks in reverse so
/// multi-turn inputs return the most recent prompt. `input` may be a
/// string (simplified single-prompt mode) or an array (canonical mode);
/// both are handled here so callers don't have to peek at the shape.
pub fn extract_user_input(req: &Value) -> Option<String> {
    match req.get("input")? {
        Value::Array(items) => {
            for it in items.iter().rev() {
                if it.get("type").and_then(|v| v.as_str()) != Some("message") {
                    continue;
                }
                if it.get("role").and_then(|v| v.as_str()) != Some("user") {
                    continue;
                }
                if let Some(t) = message_text(it) {
                    return Some(t);
                }
            }
            None
        }
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// True iff the last item in `input[]` is a non-empty `role=user` message
/// (i.e. user turn, not a tool roundtrip continuation).
/// `function_call_output` items at the tail break user-turn-start.
pub fn is_user_turn_start(req: &Value) -> Option<bool> {
    let items = match req.get("input")? {
        Value::Array(items) => items,
        Value::String(s) => return Some(!s.trim().is_empty()),
        _ => return None,
    };
    let last = items.last()?;
    let t = last.get("type").and_then(|v| v.as_str());
    if t != Some("message") || last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return Some(false);
    }
    Some(message_text(last).is_some())
}

/// First `message` item's text from `output[]`. `None` if the response
/// only contains tool/function calls or `reasoning` items.
pub fn extract_assistant_text(body: &str) -> Option<String> {
    let resp: Value = serde_json::from_str(body).ok()?;
    extract_assistant_text_value(&resp)
}

/// `Value`-input sibling of `extract_assistant_text`. Used by agent-profile
/// extractors on the parse-once hot path.
pub fn extract_assistant_text_value(resp: &Value) -> Option<String> {
    let output = resp.get("output")?.as_array()?;
    for it in output {
        if it.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        if let Some(t) = message_text(it) {
            return Some(t);
        }
    }
    None
}

// ─────────────────────────── Token estimation ─────────────────────────────

use crate::token_estimator::{collect_responses_output_text, TokenEstimator};

/// True iff the response body carried any Responses-shape `usage` field.
pub fn usage_present(body: &Value) -> bool {
    let u = match body.get("usage") {
        Some(v) => v,
        None => return false,
    };
    if !u.is_object() {
        return false;
    }
    u.get("input_tokens")
        .and_then(|v| v.as_u64())
        .map(|n| n > 0)
        .unwrap_or(false)
        || u.get("output_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n > 0)
            .unwrap_or(false)
}

/// Walk the top-level `output[*]` array and produce one concatenated, deduped
/// string of all reasoning summary + reasoning content + assistant message
/// text + serialized function_call/tool_call payloads. Returns empty if
/// `output` is missing or wrong-typed.
pub fn collected_response_text(body: &Value) -> String {
    if let Some(out) = body.get("output") {
        return collect_responses_output_text(out);
    }
    String::new()
}

/// Estimate input tokens for an OpenAI Responses request. Walks
/// `instructions` (system prompt), `input[*]` messages (with role + content
/// array), and `tools[*]`.
pub fn estimate_input_tokens(req: &Value, est: &dyn TokenEstimator) -> u32 {
    let mut total: u32 = 0;

    if let Some(s) = req.get("instructions").and_then(|v| v.as_str()) {
        total = total.saturating_add(est.count_text(s));
    }

    if let Some(items) = req.get("input").and_then(|v| v.as_array()) {
        for it in items {
            // Items can be:
            //   {role:"...", content:[{type:"input_text"|"output_text", text}]}
            //   {type:"function_call_output", output:"..."}
            //   {type:"function_call", name, arguments}
            //   {type:"reasoning", ...}  (rare on input but possible if echoed)
            if let Some(role) = it.get("role").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(role));
            }
            if let Some(content) = it.get("content").and_then(|v| v.as_array()) {
                for c in content {
                    if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                        total = total.saturating_add(est.count_text(t));
                    }
                }
            } else if let Some(s) = it.get("content").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(s));
            }
            // function_call_output: count `output` field
            if let Some(out) = it.get("output").and_then(|v| v.as_str()) {
                total = total.saturating_add(est.count_text(out));
            }
            // function_call: serialize whole
            if it.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                if let Ok(s) = serde_json::to_string(it) {
                    total = total.saturating_add(est.count_text(&s));
                }
            }
        }
    }

    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            if let Ok(s) = serde_json::to_string(t) {
                total = total.saturating_add(est.count_text(&s));
            }
        }
    }
    if let Some(tc) = req.get("tool_choice") {
        if let Ok(s) = serde_json::to_string(tc) {
            total = total.saturating_add(est.count_text(&s));
        }
    }

    total
}

/// Estimate output tokens for an OpenAI Responses response body. Counts
/// reasoning summary + reasoning content + message text + tool/function
/// call payloads, all deduplicated.
pub fn estimate_output_tokens(resp: &Value, est: &dyn TokenEstimator) -> u32 {
    let text = collected_response_text(resp);
    est.count_text(&text)
}

#[cfg(test)]
mod terminal_helper_tests {
    use super::*;

    #[test]
    fn terminal_when_output_only_has_message() {
        let body = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}]}"#;
        assert!(body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_function_call_present() {
        let body = r#"{"output":[{"type":"message"},{"type":"function_call","call_id":"fc_1"}]}"#;
        assert!(!body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_no_message() {
        let body = r#"{"output":[{"type":"reasoning"}]}"#;
        assert!(!body_has_terminal_message_only(Some(body)));
    }

    #[test]
    fn not_terminal_when_no_body() {
        assert!(!body_has_terminal_message_only(None));
    }

    #[test]
    fn not_terminal_when_malformed_json() {
        assert!(!body_has_terminal_message_only(Some("garbage")));
    }

    #[test]
    fn not_terminal_when_output_array_is_empty() {
        assert!(!body_has_terminal_message_only(Some(r#"{"output":[]}"#)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use h_protocol::net::FlowKey;

    fn make_sse(event_type: &str, data: &str) -> SseEventData {
        let ip: IpAddr = IpAddr::from([127, 0, 0, 1]);
        SseEventData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            event_type: event_type.to_string(),
            data: data.to_string(),
            timestamp_us: 0,
            process: None,
        }
    }

    #[test]
    fn test_grafts_output_items_into_empty_response_output() {
        // Real-codex behavior: response.completed.response.output is empty;
        // items arrive via response.output_item.done. Parser must accumulate
        // them and graft into the response_body so downstream consumers
        // (turn-terminal predicate, assistant-text extraction) can inspect
        // them.
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
        let (info, _body) = extract_sse(&events);
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
    fn test_preserves_nonempty_response_output() {
        // If response.completed already carries output (older/alternative
        // codex behavior), we must NOT overwrite it with accumulated items.
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
        let (info, _body) = extract_sse(&events);
        let body = info.response_body.expect("response_body");
        let v: Value = serde_json::from_str(&body).unwrap();
        let out = v.get("output").and_then(|o| o.as_array()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("type").and_then(|t| t.as_str()), Some("message"));
    }

    #[test]
    fn test_extract_response_id_streaming() {
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
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("resp_xyz"));
    }

    #[test]
    fn test_extract_sse_cache_read_tokens() {
        // Responses API carries cached_tokens under usage.input_tokens_details.
        let events = vec![make_sse(
            "response.completed",
            r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed","usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110,"input_tokens_details":{"cached_tokens":42}}}}"#,
        )];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.cache_read_input_tokens, Some(42));
    }

    #[test]
    fn test_extract_response_non_streaming() {
        // Non-streaming Responses body: same shape as the inner `response`
        // object inside a response.completed SSE event.
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
                "input_tokens_details": {"cached_tokens": 3}
            }
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
            process: None,
        };
        let cache = ParsedJson::from_bytes(resp.body.clone());
        let info = extract_response(&resp, &cache);
        assert_eq!(info.model.as_deref(), Some("gpt-5"));
        assert_eq!(info.response_id.as_deref(), Some("resp_1"));
        assert_eq!(info.finish_reason.as_deref(), Some("completed"));
        assert_eq!(info.input_tokens, Some(10));
        assert_eq!(info.output_tokens, Some(5));
        assert_eq!(info.total_tokens, Some(15));
        assert_eq!(info.cache_read_input_tokens, Some(3));
    }

    #[test]
    fn predicates_openai_responses() {
        let w = OpenAiResponsesWireApi;
        assert!(w.is_terminal("completed"));
        assert!(w.is_terminal("incomplete"));
        assert!(w.is_terminal("failed"));
        assert!(w.is_terminal("cancelled"));
        assert!(!w.is_terminal("in_progress"));
        assert!(!w.is_terminal("unknown_future_value"));
        // Responses surfaces tool use via output items, not finish_reason —
        // predicate is false even for known terminals.
        assert!(!w.is_tool_use("completed"));
        assert!(!w.is_tool_use("failed"));
        assert!(!w.is_tool_use("tool_calls"));
    }

    #[test]
    fn test_extract_sse_ignores_chat_chunks() {
        // Defensive: if a Chat-style untyped chunk slipped in, we skip it.
        let events = vec![
            make_sse(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            ),
            make_sse(
                "response.completed",
                r#"{"response":{"id":"resp_1","model":"gpt-5","status":"completed"}}"#,
            ),
        ];
        let (info, _body) = extract_sse(&events);
        assert_eq!(info.model.as_deref(), Some("gpt-5"));
        assert_eq!(info.response_id.as_deref(), Some("resp_1"));
    }
}

#[cfg(test)]
mod estimator_tests {
    use super::*;
    use crate::token_estimator::CL100kEstimator;
    use serde_json::json;

    fn cl() -> CL100kEstimator {
        CL100kEstimator::new()
    }

    #[test]
    fn usage_present_true_when_set() {
        let body = json!({"usage": {"input_tokens": 10, "output_tokens": 5}});
        assert!(usage_present(&body));
    }

    #[test]
    fn usage_present_false_when_missing_or_zero() {
        assert!(!usage_present(&json!({})));
        assert!(!usage_present(
            &json!({"usage": {"input_tokens": 0, "output_tokens": 0}})
        ));
    }

    #[test]
    fn estimate_input_walks_instructions_and_input_messages() {
        let req = json!({
            "instructions":"You are helpful",
            "input":[
                {"role":"user","content":[
                    {"type":"input_text","text":"Hello"}
                ]}
            ],
            "tools":[
                {"type":"function","name":"calc","description":"adds","parameters":{}}
            ]
        });
        let n = estimate_input_tokens(&req, &cl());
        assert!(n > 5);
    }

    #[test]
    fn estimate_input_function_call_output() {
        let req = json!({
            "input":[
                {"type":"function_call_output","call_id":"call_x","output":"the result"}
            ]
        });
        let n = estimate_input_tokens(&req, &cl());
        assert!(n > 0);
    }

    #[test]
    fn estimate_output_reasoning_summary_plus_message() {
        let resp = json!({
            "output":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"thoughts"}]},
                {"type":"message","content":[{"type":"output_text","text":"final"}]}
            ]
        });
        let n = estimate_output_tokens(&resp, &cl());
        let plain = estimate_output_tokens(
            &json!({"output":[{"type":"message","content":[{"type":"output_text","text":"final"}]}]}),
            &cl(),
        );
        assert!(
            n > plain,
            "reasoning summary must add tokens (got {n} vs plain {plain})"
        );
    }

    #[test]
    fn estimate_output_dedupes_reasoning_summary_text() {
        let resp = json!({
            "output":[
                {"type":"reasoning","summary":[
                    {"text":"trace"},
                    {"text":"trace"}
                ]},
                {"type":"message","content":[{"type":"output_text","text":"ok"}]}
            ]
        });
        let single = json!({
            "output":[
                {"type":"reasoning","summary":[{"text":"trace"}]},
                {"type":"message","content":[{"type":"output_text","text":"ok"}]}
            ]
        });
        assert_eq!(
            estimate_output_tokens(&resp, &cl()),
            estimate_output_tokens(&single, &cl())
        );
    }

    #[test]
    fn estimate_output_function_call_counted() {
        let resp = json!({
            "output":[
                {"type":"function_call","name":"do","arguments":"{}","call_id":"call_y"}
            ]
        });
        assert!(estimate_output_tokens(&resp, &cl()) > 0);
    }

    #[test]
    fn estimate_output_zero_on_empty_or_unexpected() {
        assert_eq!(estimate_output_tokens(&json!({}), &cl()), 0);
        assert_eq!(estimate_output_tokens(&json!({"output":[]}), &cl()), 0);
    }

    // ─── first_user_text: strict "first role:user" semantics ────────────────

    #[test]
    fn first_user_text_returns_text_of_first_user_message() {
        let items = vec![
            json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"sys"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"more"}]}),
        ];
        assert_eq!(first_user_text(&items).as_deref(), Some("hello"));
    }

    #[test]
    fn first_user_text_none_when_first_user_has_no_text() {
        // First user item has empty content array; second user has text.
        // Strict semantics: don't fall through.
        let items = vec![
            json!({"type":"message","role":"user","content":[]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"later"}]}),
        ];
        assert_eq!(first_user_text(&items), None);
    }

    // ─── first_assistant_sig_from_input: window bounded to turn 1 ───────────

    #[test]
    fn first_assistant_sig_from_input_stable_across_turn_2_function_call() {
        // Turn 1 was text-only (no function_call). Turn 2 introduces a
        // function_call. The sig must still come from turn 1's text — an
        // unbounded scan would jump to turn 2's fc and break session_id
        // stability across calls.
        let items_call_2 = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"u1"}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"u2"}]}),
        ];
        let items_call_3 = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"u1"}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"u2"}]}),
            json!({"type":"reasoning","summary":[],"content":[]}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_t2"}),
            json!({"type":"function_call_output","call_id":"fc_t2","output":"ok"}),
        ];
        let sig_2 = first_assistant_sig_from_input(&items_call_2).unwrap();
        let sig_3 = first_assistant_sig_from_input(&items_call_3).unwrap();
        assert!(matches!(&sig_2, AssistantSig::Text(t) if t == "hello"));
        assert!(matches!(&sig_3, AssistantSig::Text(t) if t == "hello"));
    }

    #[test]
    fn first_assistant_sig_from_input_picks_fc_within_first_turn() {
        // Turn 1 has text + function_call; turn 2 also has function_call.
        // The bounded scan must pick turn 1's fc, not turn 2's.
        let items = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"u1"}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_t1"}),
            json!({"type":"function_call_output","call_id":"fc_t1","output":"ok"}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"u2"}]}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_t2"}),
        ];
        let sig = first_assistant_sig_from_input(&items).unwrap();
        assert!(matches!(sig, AssistantSig::ToolId(id) if id == "fc_t1"));
    }

    // ─── first_assistant_sig_from_response_value: direct generation scan ──────

    #[test]
    fn first_assistant_sig_from_response_value_picks_fc_over_text() {
        // Standard response: reasoning + assistant text + function_call.
        // The response path scans the generation directly (no input bounding)
        // and applies the same fc > text precedence.
        let resp = json!({"output":[
            {"type":"reasoning","summary":[],"content":[]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_resp"},
        ]});
        let sig = first_assistant_sig_from_response_value(&resp).unwrap();
        assert!(matches!(sig, AssistantSig::ToolId(id) if id == "fc_resp"));
    }

    #[test]
    fn first_assistant_sig_from_response_value_falls_back_to_text_when_no_fc() {
        let resp = json!({"output":[
            {"type":"reasoning","summary":[],"content":[]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"final"}]},
        ]});
        let sig = first_assistant_sig_from_response_value(&resp).unwrap();
        assert!(matches!(sig, AssistantSig::Text(t) if t == "final"));
    }

    #[test]
    fn first_assistant_sig_from_response_value_does_not_apply_input_bounding() {
        // Pathological output that mixes input-shaped items (a stray
        // function_call_output, a stray user message) into a response.
        // The response path must NOT short-circuit at the fco (which is
        // input-only generation boundary) or skip leading user items;
        // it just scans the whole array and picks fc > text. This guards
        // against re-coupling response_value back to input's bounding logic.
        let resp = json!({"output":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"intro"}]},
            {"type":"function_call_output","call_id":"fc_dummy","output":"x"},
            {"type":"function_call","name":"f","arguments":"{}","call_id":"fc_after_fco"},
        ]});
        let sig = first_assistant_sig_from_response_value(&resp).unwrap();
        // input bounding would have stopped at the fco and returned Text(intro);
        // response scan ignores fco semantics → picks fc_after_fco.
        assert!(matches!(sig, AssistantSig::ToolId(id) if id == "fc_after_fco"));
    }

    #[test]
    fn first_assistant_sig_from_input_codex_dual_user_prefix() {
        // Codex CLI splits turn 1's user side into two consecutive
        // `role:user` items: a synthetic environment_context, then the real
        // prompt. The leading user run must be skipped past entirely;
        // generation 1 starts at the assistant generation AFTER both users.
        // Sig must be stable across calls within turn 1 and across turn 2.
        let items_call_1_resp_only = vec![
            // Sanity-equivalent of what call 1's response output gives us:
            json!({"type":"reasoning","summary":[],"content":[]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_e1"}),
        ];
        let sig_call_1 = first_assistant_sig_from_input(&items_call_1_resp_only).unwrap();

        let items_call_2 = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"environment_context"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"real prompt"}]}),
            json!({"type":"reasoning","summary":[],"content":[]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_e1"}),
            json!({"type":"function_call_output","call_id":"fc_e1","output":"ok"}),
        ];
        let sig_call_2 = first_assistant_sig_from_input(&items_call_2).unwrap();

        let items_turn_2_call = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"environment_context"}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"real prompt"}]}),
            json!({"type":"reasoning","summary":[],"content":[]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_e1"}),
            json!({"type":"function_call_output","call_id":"fc_e1","output":"ok"}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"follow-up"}]}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_t2"}),
        ];
        let sig_turn_2 = first_assistant_sig_from_input(&items_turn_2_call).unwrap();

        for sig in [&sig_call_1, &sig_call_2, &sig_turn_2] {
            assert!(matches!(sig, AssistantSig::ToolId(id) if id == "fc_e1"));
        }
    }

    #[test]
    fn first_assistant_sig_from_input_generation_1_text_only_does_not_leak_to_generation_2() {
        // Generation 1 is text-only; generation 2 (within the same turn 1, after
        // an fco) carries a function_call. Strict "generation 1 only" semantics
        // means we must return Text(generation_1_msg), not ToolId(fc_e2).
        // (This shape only arises with non-standard agents that loop through
        // a turn without producing a final terminal generation, but the bound
        // is what guarantees session_id stability when it does.)
        let items = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"u1"}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"intermediate"}]}),
            json!({"type":"function_call_output","call_id":"fc_dummy","output":"x"}),
            json!({"type":"function_call","name":"f","arguments":"{}","call_id":"fc_e2"}),
        ];
        let sig = first_assistant_sig_from_input(&items).unwrap();
        assert!(matches!(sig, AssistantSig::Text(t) if t == "intermediate"));
    }
}
