//! Gemini AI Studio wire API (`POST /v1beta/models/{model}:generateContent` and
//! `:streamGenerateContent`, served via `generativelanguage.googleapis.com` or
//! a transparent gateway). The body schema follows the public Google GenAI SDK
//! (`@google/genai`, `google-genai-sdk/*`), keyed via `x-goog-api-key`.
//!
//! Distinct from the Code Assist OAuth variant (future `gemini-codeassist`),
//! whose path is `/v1internal:generateContent` and whose body wraps `contents`
//! under a `request` envelope. Detection here rejects that variant via two
//! gates: the `/v1beta/models/` path prefix and the no-`request`-at-top-level
//! shape predicate.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use h_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};

use crate::model::{RequestInfo, ResponseInfo, RouteVerdict, WireApi};
use crate::parsed_json::ParsedJson;
use crate::wire_apis::AssistantSig;

/// Synthetic finish-reason emitted when `finishReason == "STOP"` and the
/// response contains a `functionCall` part. Gemini has no native tool-use
/// signal, so we surface one for downstream turn tracking. The original
/// `STOP` is preserved verbatim inside `response_body.candidates[0].finishReason`.
pub const SYNTHETIC_TOOL_USE: &str = "TOOL_USE";

pub struct GeminiAiStudioWireApi;

impl WireApi for GeminiAiStudioWireApi {
    fn name(&self) -> &'static str {
        super::GEMINI_AISTUDIO
    }

    fn classify_route(&self, req: &HttpRequestData) -> RouteVerdict {
        if req.method != "POST" {
            return RouteVerdict::Reject;
        }
        let path = req.uri.split('?').next().unwrap_or(&req.uri);

        // Two-gate route check: AI Studio path prefix (locks shape) AND
        // generateContent/streamGenerateContent action verb. The verb alone
        // is shared with Code Assist OAuth (`/v1internal:generateContent`)
        // which we must not absorb here.
        let has_aistudio_prefix = path.contains("/v1beta/models/");
        let has_generate_verb =
            path.ends_with(":generateContent") || path.ends_with(":streamGenerateContent");

        if has_aistudio_prefix && has_generate_verb {
            return RouteVerdict::Accept;
        }
        RouteVerdict::Unknown
    }

    fn matches_shape(&self, _req: &HttpRequestData, req_body: &ParsedJson) -> bool {
        let Some(body) = req_body.get() else {
            return false;
        };
        // Anti-signals: OpenAI/Anthropic top-level `messages`, OpenAI Responses
        // `input`, Code Assist OAuth wrap `request` (which holds the real
        // contents one level down).
        if body.get("messages").is_some()
            || body.get("input").is_some()
            || body.get("request").is_some()
        {
            return false;
        }
        // Required: non-empty top-level `contents` array.
        body.get("contents")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
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
            "STOP"
                | "MAX_TOKENS"
                | "SAFETY"
                | "RECITATION"
                | "LANGUAGE"
                | "BLOCKLIST"
                | "PROHIBITED_CONTENT"
                | "SPII"
                | "MALFORMED_FUNCTION_CALL"
                | "IMAGE_SAFETY"
                | "OTHER"
                | SYNTHETIC_TOOL_USE
        )
    }

    fn is_tool_use(&self, finish_reason: &str) -> bool {
        // Only the synthesized signal — `MALFORMED_FUNCTION_CALL` is an error
        // termination, not a tool-result handshake.
        finish_reason == SYNTHETIC_TOOL_USE
    }
}

// ─────────────────────────────── request ──────────────────────────────────

fn extract_request(req: &HttpRequestData, _req_body: &ParsedJson) -> RequestInfo {
    let (model, action) = parse_path_model_action(&req.uri);
    let is_stream = action == "streamGenerateContent";
    RequestInfo {
        model: model.unwrap_or_else(|| "unknown".to_string()),
        is_stream,
    }
}

/// Parse `(model, action)` from a Gemini AI Studio URI.
/// Returns `(None, "")` when the URI doesn't match the expected shape.
fn parse_path_model_action(uri: &str) -> (Option<String>, &'static str) {
    let path = uri.split('?').next().unwrap_or(uri);
    let last_seg = path.rsplit('/').next().unwrap_or("");
    let mut split = last_seg.splitn(2, ':');
    let model = split.next().filter(|s| !s.is_empty()).map(str::to_string);
    let action = match split.next() {
        Some("generateContent") => "generateContent",
        Some("streamGenerateContent") => "streamGenerateContent",
        _ => "",
    };
    (model, action)
}

// ─────────────────────────── non-streaming response ───────────────────────

fn extract_response(resp: &HttpResponseData, resp_body: &ParsedJson) -> ResponseInfo {
    let parsed = resp_body.get();
    let body_str = std::str::from_utf8(&resp.body).ok().map(|s| s.to_string());

    let response_id = parsed
        .and_then(|b| b.get("responseId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let model = parsed
        .and_then(|b| b.get("modelVersion").or_else(|| b.get("model")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let candidates = parsed
        .and_then(|b| b.get("candidates"))
        .and_then(|v| v.as_array());
    let first_candidate = candidates.and_then(|a| a.first());

    let raw_finish = first_candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let has_function_call = first_candidate
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| parts.iter().any(|p| p.get("functionCall").is_some()))
        .unwrap_or(false);
    let finish_reason = synthesize_finish_reason(raw_finish, has_function_call);

    let usage = parsed.and_then(|b| b.get("usageMetadata"));
    let (input_tokens, output_tokens, total_tokens, cache_read_input_tokens) = extract_usage(usage);

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

/// `STOP` + functionCall present ⇒ synthesize TOOL_USE; otherwise pass
/// through wire value verbatim.
fn synthesize_finish_reason(raw: Option<String>, has_function_call: bool) -> Option<String> {
    match raw {
        Some(ref s) if s == "STOP" && has_function_call => Some(SYNTHETIC_TOOL_USE.to_string()),
        other => other,
    }
}

/// Map `usageMetadata` → (input, output, total, cache_read).
/// `thoughtsTokenCount` is intentionally not surfaced: per Gemini spec it is
/// already a subset of `candidatesTokenCount`, so adding it would double-count.
fn extract_usage(usage: Option<&Value>) -> (Option<u32>, Option<u32>, Option<u32>, Option<u32>) {
    let u = match usage {
        Some(v) => v,
        None => return (None, None, None, None),
    };
    let input = u
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let output = u
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let total = u
        .get("totalTokenCount")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let cache = u
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    (input, output, total, cache)
}

// ───────────────────────────── streaming SSE ──────────────────────────────

fn extract_sse(events: &[SseEventData]) -> (ResponseInfo, ParsedJson) {
    // Each Gemini chunk is `data: {...}` only (no `event:` line), so
    // event_type is empty and we dispatch purely by JSON content. Parse
    // each event exactly once and reuse below.
    let parsed: Vec<Value> = events
        .iter()
        .map(|e| serde_json::from_str(&e.data).unwrap_or(Value::Null))
        .collect();

    let synthetic = build_response_body(&parsed);

    let response_id = synthetic
        .as_ref()
        .and_then(|v| v.get("responseId"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let model = synthetic
        .as_ref()
        .and_then(|v| v.get("modelVersion").or_else(|| v.get("model")))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let usage = synthetic.as_ref().and_then(|v| v.get("usageMetadata"));
    let (input_tokens, output_tokens, total_tokens, cache_read_input_tokens) = extract_usage(usage);

    let candidates = synthetic
        .as_ref()
        .and_then(|v| v.get("candidates"))
        .and_then(|v| v.as_array());
    let first_candidate = candidates.and_then(|a| a.first());
    let raw_finish = first_candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let has_function_call = first_candidate
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| parts.iter().any(|p| p.get("functionCall").is_some()))
        .unwrap_or(false);
    let finish_reason = synthesize_finish_reason(raw_finish, has_function_call);

    let response_body = synthetic.as_ref().map(Value::to_string);
    let response_cache = ParsedJson::from_value(synthetic);

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

/// Per-candidate accumulation state. Gemini chunks emit incremental
/// `parts[]` entries — adjacent text fragments collapse into one bucket
/// per kind to keep the synthesized body compact, while `functionCall` /
/// `inlineData` are pushed verbatim because each is already a complete
/// part on the wire (no partial-arg streaming).
#[derive(Default)]
struct PartialCandidate {
    role: Option<String>,
    text: String,
    thought: String,
    discrete_parts: Vec<Value>,
    finish_reason: Option<String>,
    index: Option<u64>,
}

/// Reassemble the streaming chunks into a single response body matching
/// the non-streaming schema: `{candidates, modelVersion, responseId,
/// usageMetadata}`. Returns `None` when no chunk produced a usable
/// candidate or top-level field — same contract as the Anthropic
/// equivalent in `wire_apis/anthropic.rs`.
fn build_response_body(parsed: &[Value]) -> Option<Value> {
    let mut candidates: BTreeMap<u64, PartialCandidate> = BTreeMap::new();
    let mut response_id: Option<String> = None;
    let mut model_version: Option<String> = None;
    let mut usage_metadata: Option<Value> = None;
    let mut any_data = false;

    for chunk in parsed {
        if !chunk.is_object() {
            continue;
        }
        any_data = true;

        if let Some(s) = chunk.get("responseId").and_then(|v| v.as_str()) {
            response_id = Some(s.to_string());
        }
        if let Some(s) = chunk
            .get("modelVersion")
            .or_else(|| chunk.get("model"))
            .and_then(|v| v.as_str())
        {
            model_version = Some(s.to_string());
        }
        // Gemini ships cumulative usage snapshots; later chunks supersede
        // earlier ones. Take the last non-null occurrence.
        if let Some(usage) = chunk.get("usageMetadata") {
            if !usage.is_null() {
                usage_metadata = Some(usage.clone());
            }
        }

        let Some(arr) = chunk.get("candidates").and_then(|v| v.as_array()) else {
            continue;
        };
        for cand in arr {
            let idx = cand.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            let entry = candidates.entry(idx).or_default();
            entry.index = Some(idx);
            if let Some(role) = cand
                .get("content")
                .and_then(|c| c.get("role"))
                .and_then(|r| r.as_str())
            {
                entry.role = Some(role.to_string());
            }
            if let Some(fr) = cand.get("finishReason").and_then(|v| v.as_str()) {
                entry.finish_reason = Some(fr.to_string());
            }
            let parts = cand
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|v| v.as_array());
            if let Some(parts) = parts {
                for p in parts {
                    accumulate_part(entry, p);
                }
            }
        }
    }

    if !any_data && candidates.is_empty() {
        return None;
    }

    let candidates_json: Vec<Value> = candidates.into_values().map(finalize_candidate).collect();

    let mut out = json!({});
    let obj = out.as_object_mut().unwrap();
    obj.insert("candidates".to_string(), Value::Array(candidates_json));
    if let Some(id) = response_id {
        obj.insert("responseId".to_string(), Value::String(id));
    }
    if let Some(m) = model_version {
        obj.insert("modelVersion".to_string(), Value::String(m));
    }
    if let Some(u) = usage_metadata {
        obj.insert("usageMetadata".to_string(), u);
    }
    Some(out)
}

fn accumulate_part(entry: &mut PartialCandidate, part: &Value) {
    if part.get("functionCall").is_some() || part.get("inlineData").is_some() {
        // Discrete parts: each is a complete unit (functionCall args are
        // always shipped whole, inlineData is a full base64 blob). Keep
        // them in arrival order alongside any text/thought aggregation.
        entry.discrete_parts.push(part.clone());
        return;
    }
    let is_thought = part
        .get("thought")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
        if is_thought {
            entry.thought.push_str(t);
        } else {
            entry.text.push_str(t);
        }
        return;
    }
    // Unknown part shape — preserve verbatim so we don't silently drop data.
    entry.discrete_parts.push(part.clone());
}

fn finalize_candidate(p: PartialCandidate) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    if !p.thought.is_empty() {
        parts.push(json!({"thought": true, "text": p.thought}));
    }
    if !p.text.is_empty() {
        parts.push(json!({"text": p.text}));
    }
    parts.extend(p.discrete_parts);

    let mut content = json!({"parts": parts});
    if let Some(role) = p.role {
        content
            .as_object_mut()
            .unwrap()
            .insert("role".to_string(), Value::String(role));
    }

    let mut out = json!({"content": content});
    let obj = out.as_object_mut().unwrap();
    if let Some(idx) = p.index {
        obj.insert("index".to_string(), json!(idx));
    }
    if let Some(fr) = p.finish_reason {
        obj.insert("finishReason".to_string(), Value::String(fr));
    }
    out
}

// ────────────────────────── body-shape parsers ────────────────────────────
//
// Helpers that walk a Gemini AI Studio request / response body to pull out
// the pieces agent profiles need (first/last user text, assistant signature,
// system prompt). All operate on a parsed `serde_json::Value`.
//
// Conventions:
//   * `parts[]` of role==user content can mix `text`, `inlineData`, and
//     `functionResponse` (tool roundtrip). User text helpers ignore
//     `functionResponse` parts so a tool-result-only continuation doesn't
//     masquerade as a fresh user input.
//   * `parts[]` of role==model content can mix `text`, `thought:true text`,
//     and `functionCall`. The assistant-sig path **strips `thought` parts**:
//     thought never survives client-side echo into the next request's
//     `contents[]`, so signing over thought would break sig consistency
//     between `first_assistant_sig_from_response_value` (current call) and
//     `first_assistant_sig_from_request` (next call's history).
//   * `functionCall.id` is optional in the Gemini protocol — server may
//     issue, otherwise Gemini CLI synthesizes `{name}_{ms}_{counter}` only
//     in `functionResponse` echoes (NOT on the assistant turn). So sigs
//     must work without relying on id presence.

/// Concatenate all visible text parts (text, with `thought != true`) from
/// a content block's `parts[]`. Returns `String::new()` when no visible
/// text exists.
fn visible_text_of_parts(parts: &[Value]) -> String {
    let mut out = String::new();
    for p in parts {
        if p.get("thought").and_then(|t| t.as_bool()) == Some(true) {
            continue;
        }
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    out
}

/// Canonical JSON serialization with object keys sorted lexicographically.
/// Required for sig stability: the same `args` object can come from the
/// streaming response (server's preferred key order) or from the next
/// request's `contents[]` (whatever order Gemini CLI rebuilds it in), and
/// both must hash identically. Self-walking the `Value` instead of relying
/// on `serde_json::to_string` because Map's key order behavior depends on
/// the `preserve_order` cargo feature.
fn canonical_json(value: &Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => {
                let _ = write!(out, "{n}");
            }
            Value::String(s) => {
                // `serde_json::to_string` of a String escapes correctly;
                // delegating ensures we match standard JSON escaping rules.
                let _ = write!(
                    out,
                    "{}",
                    serde_json::to_string(s).unwrap_or_else(|_| String::from("\"\""))
                );
            }
            Value::Array(arr) => {
                out.push('[');
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(v, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(
                        out,
                        "{}",
                        serde_json::to_string(*k).unwrap_or_else(|_| String::from("\"\""))
                    );
                    out.push(':');
                    write(&map[*k], out);
                }
                out.push('}');
            }
        }
    }
    write(value, &mut out);
    out
}

/// Build a stable signature over a model turn's `parts[]`. Returns
/// `(canonical_string, is_pure_text)` where:
///
///   - `canonical_string` joins one segment per non-`thought` part with `|`.
///     Per-part encoding:
///       - text         → `t:<text>`
///       - functionCall → `f:<id>` if `id` present, else `f:<name>/<canonical_args>`
///       - inlineData   → `d:<mimeType>:<data_len>`
///       - unknown      → `?:<canonical_json>` (verbatim so future part types
///         still contribute to sig stability)
///
///   - `is_pure_text` is true iff every contributing segment was `text` —
///     no functionCall, inlineData, or unknown parts. Callers wrap pure-
///     text sigs in `AssistantSig::Text` (so `generic` profile's helper-
///     shape one-shot detector keeps working as designed) and tools-bearing
///     sigs in `AssistantSig::ToolId` (so the same detector — which
///     pattern-matches `Text(_)` only — skips multi-turn agent first
///     calls).
///
/// Returns `None` when the parts array yields zero non-thought segments.
fn parts_sig(parts: &[Value]) -> Option<(String, bool)> {
    let mut segs: Vec<String> = Vec::new();
    let mut is_pure_text = true;
    for p in parts {
        if p.get("thought").and_then(|t| t.as_bool()) == Some(true) {
            continue;
        }
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            segs.push(format!("t:{t}"));
            continue;
        }
        is_pure_text = false;
        if let Some(fc) = p.get("functionCall") {
            if let Some(id) = fc.get("id").and_then(|v| v.as_str()) {
                segs.push(format!("f:{id}"));
            } else {
                let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = fc.get("args").cloned().unwrap_or(Value::Null);
                segs.push(format!("f:{name}/{}", canonical_json(&args)));
            }
            continue;
        }
        if let Some(inline) = p.get("inlineData") {
            let mime = inline
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let len = inline
                .get("data")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            segs.push(format!("d:{mime}:{len}"));
            continue;
        }
        segs.push(format!("?:{}", canonical_json(p)));
    }
    if segs.is_empty() {
        None
    } else {
        Some((segs.join("|"), is_pure_text))
    }
}

/// FNV-1a 64-bit. Used to compress a tools-bearing canonical sig string
/// (which can be hundreds of bytes for parallel tool calls + long args)
/// into a fixed-width opaque id for `AssistantSig::ToolId`. Same primitive
/// as `agents::session_id::synth_text_hash`; kept inline here to avoid a
/// cross-module helper for a one-off call site.
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Wrap a `parts_sig` result into the right `AssistantSig` variant.
/// Pure-text sigs go in `Text` (helper-shape gate eligible); tools-bearing
/// sigs go in `ToolId(tu-<16hex>)` (helper-shape gate skipped). The `tu-`
/// prefix is intentionally chosen to **not** match any of the prefixes
/// `canonicalize_tool_id` rewrites (`call`, `toolu`, `fc`, `chatcmpl`),
/// so the synthesized id passes through unmodified.
fn wrap_sig(sig_and_kind: (String, bool)) -> AssistantSig {
    let (sig, is_pure_text) = sig_and_kind;
    if is_pure_text {
        AssistantSig::Text(sig)
    } else {
        AssistantSig::ToolId(format!("tu-{:016x}", fnv1a_64(&sig)))
    }
}

/// First user content's text — used as a stable session anchor in the
/// per-conversation hash. Joins all `text` parts of the first role==user
/// content; ignores `functionResponse` and `inlineData` parts. `None`
/// when no role==user content exists or all its text is empty/whitespace.
///
/// Note: Gemini CLI prepends a synthetic `<session_context>` user content
/// in front of the real prompt. We deliberately take that one (the FIRST
/// user content) as the anchor — its scaffolding text is constant within
/// a session, which is exactly the property we want for session_id
/// stability across the conversation's calls.
pub fn first_user_text(req: &Value) -> Option<String> {
    let contents = req.get("contents")?.as_array()?;
    let first_user = contents
        .iter()
        .find(|c| c.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    let parts = first_user.get("parts").and_then(|v| v.as_array())?;
    let joined = parts
        .iter()
        .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Top-level `systemInstruction.parts[].text` joined. Gemini puts system
/// prompt in a dedicated top-level field rather than inline among contents,
/// so the helper-shape one-shot detection has a clean signal.
pub fn first_system_text(req: &Value) -> Option<String> {
    let parts = req.get("systemInstruction")?.get("parts")?.as_array()?;
    let joined = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Sig of the **first** `role:model` content in `contents[]` — and only
/// that one. `None` when no model content exists (first call, no tool
/// roundtrip yet) OR when the first model content has no extractable
/// signal (parts missing, empty, or all-`thought`). Does NOT fall through
/// to later model contents — those belong to turn 2+ and are not a stable
/// session anchor.
pub fn first_assistant_sig_from_request(req: &Value) -> Option<AssistantSig> {
    let contents = req.get("contents")?.as_array()?;
    let first_model = contents
        .iter()
        .find(|c| c.get("role").and_then(|r| r.as_str()) == Some("model"))?;
    let parts = first_model.get("parts").and_then(|v| v.as_array())?;
    parts_sig(parts).map(wrap_sig)
}

/// Sig of the first candidate's parts in a Gemini response body. Mirrors
/// `first_assistant_sig_from_request` so the sig is stable across the
/// boundary: a call's response and the next call's echoed history hash
/// to the same `AssistantSig` (same variant, same string).
pub fn first_assistant_sig_from_response_value(resp: &Value) -> Option<AssistantSig> {
    let candidates = resp.get("candidates")?.as_array()?;
    let first = candidates.first()?;
    let parts = first.get("content")?.get("parts")?.as_array()?;
    parts_sig(parts).map(wrap_sig)
}

/// Trimmed text of the **last** user-side input. Skips contents whose
/// parts are entirely `functionResponse` (those are tool-result roundtrip
/// continuations, not fresh user input). Falls back to the previous
/// user content with text. `None` when no user content has visible text.
pub fn extract_user_input(req: &Value) -> Option<String> {
    let contents = req.get("contents")?.as_array()?;
    for c in contents.iter().rev() {
        if c.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(parts) = c.get("parts").and_then(|v| v.as_array()) else {
            continue;
        };
        // Pure-functionResponse content = tool roundtrip, not user input.
        // Skip and look further back.
        let has_only_function_responses =
            !parts.is_empty() && parts.iter().all(|p| p.get("functionResponse").is_some());
        if has_only_function_responses {
            continue;
        }
        let joined = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        let trimmed = joined.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// True iff the **last** content is role==user AND has at least one
/// non-`functionResponse` part with non-empty content (text, image, …).
/// Pure-functionResponse last content means "tool roundtrip continuing
/// the same agent turn", not a fresh user-initiated turn.
pub fn is_user_turn_start(req: &Value) -> Option<bool> {
    let last = req.get("contents")?.as_array()?.last()?;
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return Some(false);
    }
    let parts = last.get("parts")?.as_array()?;
    if parts.is_empty() {
        return Some(false);
    }
    Some(parts.iter().any(|p| {
        if p.get("functionResponse").is_some() {
            return false;
        }
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            return !t.trim().is_empty();
        }
        // Non-text, non-functionResponse part (image, file, future types)
        // counts as a user-side contribution.
        p.get("inlineData").is_some()
    }))
}

/// Joined visible text of the first candidate's `parts[]` in a response
/// (thought stripped). Used by the agent-profile assistant-text extractor
/// for downstream UI rendering and post-hoc analysis.
pub fn extract_assistant_text_value(resp: &Value) -> Option<String> {
    let candidates = resp.get("candidates")?.as_array()?;
    let first = candidates.first()?;
    let parts = first.get("content")?.get("parts")?.as_array()?;
    let joined = visible_text_of_parts(parts);
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::IpAddr;
    use h_protocol::net::FlowKey;

    fn make_request(method: &str, uri: &str, body: &str) -> HttpRequestData {
        let ip: IpAddr = IpAddr::from([127, 0, 0, 1]);
        HttpRequestData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            method: method.to_string(),
            uri: uri.to_string(),
            version: 1,
            headers: vec![],
            body: Bytes::copy_from_slice(body.as_bytes()),
            timestamp_us: 0,
            process: None,
        }
    }

    fn make_response(body: &str) -> HttpResponseData {
        let ip: IpAddr = IpAddr::from([127, 0, 0, 1]);
        HttpResponseData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![],
            body: Bytes::copy_from_slice(body.as_bytes()),
            first_byte_timestamp_us: 0,
            complete_timestamp_us: 0,
            process: None,
        }
    }

    fn make_sse(data: &str) -> SseEventData {
        let ip: IpAddr = IpAddr::from([127, 0, 0, 1]);
        SseEventData {
            flow_key: FlowKey::new(String::new(), ip, 1234, ip, 8080),
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            // Gemini SSE has no `event:` line — empty event_type is canonical.
            event_type: String::new(),
            data: data.to_string(),
            timestamp_us: 0,
            process: None,
        }
    }

    #[test]
    fn test_route_accept_canonical_paths() {
        let api = GeminiAiStudioWireApi;
        let r = make_request(
            "POST",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            "",
        );
        assert!(matches!(api.classify_route(&r), RouteVerdict::Accept));

        let r = make_request(
            "POST",
            "/v1beta/models/gemini-1.5-flash:generateContent",
            "",
        );
        assert!(matches!(api.classify_route(&r), RouteVerdict::Accept));
    }

    #[test]
    fn test_route_accept_gateway_prefix() {
        // Reverse-proxy that prefixes the canonical path is still detectable.
        let api = GeminiAiStudioWireApi;
        let r = make_request(
            "POST",
            "/api/proxy/v1beta/models/gemini-2.5-pro:generateContent",
            "",
        );
        assert!(matches!(api.classify_route(&r), RouteVerdict::Accept));
    }

    #[test]
    fn test_route_reject_non_post() {
        let api = GeminiAiStudioWireApi;
        let r = make_request("GET", "/v1beta/models/gemini-2.5-pro:generateContent", "");
        assert!(matches!(api.classify_route(&r), RouteVerdict::Reject));
    }

    #[test]
    fn test_route_oauth_codeassist_not_absorbed() {
        // /v1internal:generateContent is the Code Assist OAuth path. It
        // shares the action verb but not the /v1beta/models/ prefix, so
        // route MUST return Unknown (and shape MUST also fail). No future
        // OAuth wire-api should land in this one.
        let api = GeminiAiStudioWireApi;
        let r = make_request("POST", "/v1internal:generateContent", "");
        assert!(matches!(api.classify_route(&r), RouteVerdict::Unknown));

        let body = r#"{"model":"gemini-2.5-pro","project":"p","request":{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}}"#;
        let parsed = ParsedJson::from_bytes(Bytes::copy_from_slice(body.as_bytes()));
        assert!(!api.matches_shape(&r, &parsed));
    }

    #[test]
    fn test_shape_rejects_other_protocols() {
        let api = GeminiAiStudioWireApi;
        let r = make_request("POST", "/", "");

        let openai = r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#;
        assert!(!api.matches_shape(
            &r,
            &ParsedJson::from_bytes(Bytes::copy_from_slice(openai.as_bytes()))
        ));

        let responses = r#"{"model":"o3","input":"hi"}"#;
        assert!(!api.matches_shape(
            &r,
            &ParsedJson::from_bytes(Bytes::copy_from_slice(responses.as_bytes()))
        ));
    }

    #[test]
    fn test_shape_accepts_minimal_gemini_body() {
        let api = GeminiAiStudioWireApi;
        let r = make_request("POST", "/", "");
        let body = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
        assert!(api.matches_shape(
            &r,
            &ParsedJson::from_bytes(Bytes::copy_from_slice(body.as_bytes()))
        ));
    }

    #[test]
    fn test_extract_request_model_and_stream_flags() {
        let api = GeminiAiStudioWireApi;
        let body = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
        let cache = ParsedJson::from_bytes(Bytes::copy_from_slice(body.as_bytes()));

        let r = make_request(
            "POST",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            body,
        );
        let info = api.extract_request(&r, &cache);
        assert_eq!(info.model, "gemini-2.5-pro");
        assert!(info.is_stream);

        let r = make_request(
            "POST",
            "/v1beta/models/gemini-1.5-flash:generateContent",
            body,
        );
        let info = api.extract_request(&r, &cache);
        assert_eq!(info.model, "gemini-1.5-flash");
        assert!(!info.is_stream);
    }

    #[test]
    fn test_response_non_stream_basic_usage() {
        let api = GeminiAiStudioWireApi;
        let body = r#"{
            "responseId": "resp-abc",
            "modelVersion": "gemini-2.5-pro-001",
            "candidates": [{"index":0, "finishReason":"STOP",
                "content":{"role":"model","parts":[{"text":"hello"}]}}],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "totalTokenCount": 150,
                "cachedContentTokenCount": 30
            }
        }"#;
        let resp = make_response(body);
        let cache = ParsedJson::from_bytes(resp.body.clone());
        let info = api.extract_response(&resp, &cache);
        assert_eq!(info.response_id.as_deref(), Some("resp-abc"));
        assert_eq!(info.model.as_deref(), Some("gemini-2.5-pro-001"));
        assert_eq!(info.finish_reason.as_deref(), Some("STOP"));
        assert_eq!(info.input_tokens, Some(100));
        assert_eq!(info.output_tokens, Some(50));
        assert_eq!(info.total_tokens, Some(150));
        assert_eq!(info.cache_read_input_tokens, Some(30));
        assert!(!api.is_tool_use("STOP"));
    }

    #[test]
    fn test_response_synthesizes_tool_use_when_function_call_present() {
        let api = GeminiAiStudioWireApi;
        let body = r#"{
            "responseId": "resp-tool",
            "modelVersion": "gemini-2.5-pro",
            "candidates": [{"index":0, "finishReason":"STOP",
                "content":{"role":"model","parts":[
                    {"text":"Let me look that up."},
                    {"functionCall":{"name":"search","args":{"q":"hi"}}}
                ]}}]
        }"#;
        let resp = make_response(body);
        let cache = ParsedJson::from_bytes(resp.body.clone());
        let info = api.extract_response(&resp, &cache);
        assert_eq!(info.finish_reason.as_deref(), Some(SYNTHETIC_TOOL_USE));
        assert!(api.is_tool_use(SYNTHETIC_TOOL_USE));
        assert!(!api.is_tool_use("STOP"));
        // raw STOP must remain in the stored body for downstream tools.
        assert!(info.response_body.as_deref().unwrap().contains("\"STOP\""));
    }

    #[test]
    fn test_is_terminal_covers_all_documented_finish_reasons() {
        let api = GeminiAiStudioWireApi;
        for fr in [
            "STOP",
            "MAX_TOKENS",
            "SAFETY",
            "RECITATION",
            "LANGUAGE",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
            "SPII",
            "MALFORMED_FUNCTION_CALL",
            "IMAGE_SAFETY",
            "OTHER",
            SYNTHETIC_TOOL_USE,
        ] {
            assert!(api.is_terminal(fr), "{fr} should be terminal");
        }
        assert!(!api.is_terminal(""));
        // MALFORMED_FUNCTION_CALL is an error termination, NOT a tool-use
        // handshake — must not loop into a tool-result step.
        assert!(!api.is_tool_use("MALFORMED_FUNCTION_CALL"));
    }

    #[test]
    fn test_sse_aggregates_text_thought_and_function_calls() {
        // Mirrors the real pcap shape: a thinking preamble, no visible
        // text, then 3 parallel functionCalls in a single later chunk
        // with finishReason=STOP. We expect TOOL_USE synthesis and
        // intact functionCall preservation.
        let api = GeminiAiStudioWireApi;
        let events = vec![
            make_sse(
                r#"{"candidates":[{"index":0,"content":{"role":"model","parts":[{"thought":true,"text":"thinking-"}]}}],"modelVersion":"glm-5","responseId":"resp-1"}"#,
            ),
            make_sse(
                r#"{"candidates":[{"index":0,"content":{"role":"model","parts":[{"thought":true,"text":"about-it"}]}}]}"#,
            ),
            make_sse(
                r#"{"candidates":[{"index":0,"content":{"role":"model","parts":[
                {"functionCall":{"name":"grep","args":{"q":"x"}}},
                {"functionCall":{"name":"read","args":{"path":"/a"}}}
            ]},"finishReason":"STOP"}]}"#,
            ),
            make_sse(
                r#"{"candidates":[{"index":0,"content":{"role":"model","parts":[]}}],"usageMetadata":{"promptTokenCount":13152,"candidatesTokenCount":208,"totalTokenCount":13360,"thoughtsTokenCount":159}}"#,
            ),
        ];
        let (info, cache) = api.extract_sse(&events);
        assert_eq!(info.response_id.as_deref(), Some("resp-1"));
        assert_eq!(info.model.as_deref(), Some("glm-5"));
        assert_eq!(info.input_tokens, Some(13152));
        assert_eq!(info.output_tokens, Some(208));
        assert_eq!(info.total_tokens, Some(13360));
        // No cachedContentTokenCount in this fixture.
        assert_eq!(info.cache_read_input_tokens, None);
        // STOP + functionCall ⇒ synthesized TOOL_USE.
        assert_eq!(info.finish_reason.as_deref(), Some(SYNTHETIC_TOOL_USE));

        let body = cache.get().expect("synthetic body");
        let parts = body
            .pointer("/candidates/0/content/parts")
            .and_then(|v| v.as_array())
            .expect("parts array");
        // Expect: 1 merged thought + 0 text + 2 functionCall = 3 parts.
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts[0].get("text").and_then(|v| v.as_str()),
            Some("thinking-about-it")
        );
        assert_eq!(
            parts[0].get("thought").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(parts[1].get("functionCall").is_some());
        assert!(parts[2].get("functionCall").is_some());
        // raw STOP preserved in the synthesized body even though top-level
        // finish_reason was rewritten to TOOL_USE.
        assert_eq!(
            body.pointer("/candidates/0/finishReason")
                .and_then(|v| v.as_str()),
            Some("STOP")
        );
    }

    #[test]
    fn test_sse_usage_snapshot_takes_last_non_null() {
        let api = GeminiAiStudioWireApi;
        let events = vec![
            make_sse(
                r#"{"candidates":[{"index":0,"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}}"#,
            ),
            make_sse(
                r#"{"candidates":[{"index":0,"content":{"parts":[{"text":"b"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"cachedContentTokenCount":7,"totalTokenCount":17}}"#,
            ),
        ];
        let (info, _) = api.extract_sse(&events);
        // Last snapshot wins (Gemini sends cumulative, not delta).
        assert_eq!(info.input_tokens, Some(10));
        assert_eq!(info.output_tokens, Some(2));
        assert_eq!(info.total_tokens, Some(17));
        assert_eq!(info.cache_read_input_tokens, Some(7));
    }

    // ── body-shape parser tests ────────────────────────────────────────────

    #[test]
    fn test_first_user_text_takes_first_user_content() {
        // Mirrors real Gemini CLI shape: contents[0] is a synthetic
        // <session_context> user msg, contents[1] is the real prompt. Sig
        // anchor uses [0] because session_context is constant within a
        // session — the property we want for stable session_id grouping.
        let req: Value = serde_json::from_str(
            r#"{
            "contents": [
                {"role":"user","parts":[{"text":"<session_context>\nthis is the gemini cli"}]},
                {"role":"user","parts":[{"text":"  hello there  "}]}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(
            first_user_text(&req).as_deref(),
            Some("<session_context>\nthis is the gemini cli"),
        );
    }

    #[test]
    fn test_first_system_text_from_system_instruction() {
        let req: Value = serde_json::from_str(
            r#"{
            "systemInstruction": {"role":"user","parts":[{"text":"You are an assistant."}]},
            "contents": []
        }"#,
        )
        .unwrap();
        assert_eq!(
            first_system_text(&req).as_deref(),
            Some("You are an assistant.")
        );

        let req_empty: Value = serde_json::from_str(r#"{"contents": []}"#).unwrap();
        assert_eq!(first_system_text(&req_empty), None);
    }

    #[test]
    fn test_canonical_json_sorts_keys() {
        // Same logical args, different physical key orders → identical canonical form.
        let a: Value = serde_json::from_str(r#"{"pattern":"x","dir_path":"/a"}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"dir_path":"/a","pattern":"x"}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&a), r#"{"dir_path":"/a","pattern":"x"}"#,);

        // Recursive sort: nested objects also sort.
        let c: Value = serde_json::from_str(r#"{"z":{"b":1,"a":2},"y":[3,2,1]}"#).unwrap();
        assert_eq!(canonical_json(&c), r#"{"y":[3,2,1],"z":{"a":2,"b":1}}"#);
    }

    #[test]
    fn test_assistant_sig_resp_matches_request_history() {
        // Critical: the same model turn, viewed once as response of call N
        // and once as echoed history (contents[i].role==model) in request
        // of call N+1, must hash identically. This is the load-bearing
        // invariant for cross-call session_id stability.

        // Response side: parts have visible text + 2 functionCalls + thought
        // (thought must be stripped from the sig).
        let resp: Value = serde_json::from_str(
            r#"{
            "candidates": [{
                "index": 0,
                "content": {
                    "role": "model",
                    "parts": [
                        {"thought": true, "text": "let me think about this"},
                        {"text": "I'll search."},
                        {"functionCall": {"name": "grep", "args": {"q": "x", "path": "/a"}}},
                        {"functionCall": {"name": "read", "args": {"file": "/a"}}}
                    ]
                }
            }]
        }"#,
        )
        .unwrap();
        let sig_resp = first_assistant_sig_from_response_value(&resp).unwrap();

        // Request side: same turn echoed back as contents[2] (CLI strips thought,
        // and may have args in different key order — see canonical_json).
        let req: Value = serde_json::from_str(
            r#"{
            "contents": [
                {"role":"user","parts":[{"text":"hi"}]},
                {"role":"user","parts":[{"text":"hi2"}]},
                {"role":"model","parts":[
                    {"text": "I'll search."},
                    {"functionCall": {"name": "grep", "args": {"path": "/a", "q": "x"}}},
                    {"functionCall": {"name": "read", "args": {"file": "/a"}}}
                ]}
            ]
        }"#,
        )
        .unwrap();
        let sig_req = first_assistant_sig_from_request(&req).unwrap();

        // Tools-bearing model turn → ToolId variant (so the helper-shape
        // one-shot detector in `generic` profile skips it). Both sides
        // hash the same canonical string → same opaque id.
        let id_resp = match sig_resp {
            AssistantSig::ToolId(s) => s,
            AssistantSig::Text(_) => panic!("tools-bearing sig should be ToolId, not Text"),
        };
        let id_req = match sig_req {
            AssistantSig::ToolId(s) => s,
            AssistantSig::Text(_) => panic!("tools-bearing sig should be ToolId, not Text"),
        };
        assert_eq!(
            id_resp, id_req,
            "sig must be stable across resp/req boundary"
        );
        // tu-<16hex>: 3 + 16 = 19 chars exactly.
        assert!(id_resp.starts_with("tu-"), "got: {id_resp}");
        assert_eq!(id_resp.len(), 19, "got: {id_resp}");
    }

    #[test]
    fn test_assistant_sig_falls_back_to_function_call_when_no_visible_text() {
        // Model goes straight to functionCall with zero text preamble — sig
        // must still be non-None (else first-call session_id synthesis fails).
        // Tools-bearing → ToolId variant (helper-shape gate skipped).
        let resp: Value = serde_json::from_str(
            r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"thought": true, "text": "thinking..."},
                        {"functionCall": {"name": "grep", "args": {"q": "x"}}}
                    ]
                }
            }]
        }"#,
        )
        .unwrap();
        let sig = first_assistant_sig_from_response_value(&resp).unwrap();
        match sig {
            AssistantSig::ToolId(s) => {
                assert!(s.starts_with("tu-"), "got: {s}");
                assert_eq!(s.len(), 19);
            }
            AssistantSig::Text(_) => panic!("tools-bearing sig should be ToolId"),
        }
    }

    #[test]
    fn test_assistant_sig_pure_text_response_uses_text_variant() {
        // When the model returns ONLY text (no functionCall / inlineData),
        // sig must be Text(_) so the helper-shape one-shot detector can
        // still trigger for true single-shot helper agents.
        let resp: Value = serde_json::from_str(
            r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"thought": true, "text": "let me think"},
                        {"text": "hello there"}
                    ]
                }
            }]
        }"#,
        )
        .unwrap();
        let sig = first_assistant_sig_from_response_value(&resp).unwrap();
        match sig {
            AssistantSig::Text(s) => assert_eq!(s, "t:hello there"),
            AssistantSig::ToolId(_) => panic!("pure-text sig should be Text"),
        }
    }

    #[test]
    fn test_assistant_sig_uses_id_when_present() {
        // When server eventually issues opaque ids, the canonical string
        // uses the id directly. functionCall is still a non-text part so
        // variant is ToolId regardless.
        let resp: Value = serde_json::from_str(
            r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"id": "fc_abc123", "name": "grep", "args": {"q": "x"}}}
                    ]
                }
            }]
        }"#,
        )
        .unwrap();
        let sig = first_assistant_sig_from_response_value(&resp).unwrap();
        match sig {
            AssistantSig::ToolId(s) => {
                assert!(s.starts_with("tu-"), "got: {s}");
                assert_eq!(s.len(), 19);
            }
            AssistantSig::Text(_) => panic!("tools-bearing sig should be ToolId"),
        }
    }

    #[test]
    fn test_extract_user_input_skips_pure_function_response_content() {
        // Last content is functionResponse only (tool roundtrip continuation)
        // — extract_user_input must skip it and return the prior real prompt.
        let req: Value = serde_json::from_str(
            r#"{
            "contents": [
                {"role":"user","parts":[{"text":"<session_context>scaffold"}]},
                {"role":"user","parts":[{"text":"actual user question"}]},
                {"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]},
                {"role":"user","parts":[
                    {"functionResponse":{"name":"f","response":{"r":1},"id":"f_123_0"}}
                ]}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(
            extract_user_input(&req).as_deref(),
            Some("actual user question"),
        );
    }

    #[test]
    fn test_is_user_turn_start_false_on_function_response_only() {
        let req: Value = serde_json::from_str(
            r#"{
            "contents": [
                {"role":"user","parts":[{"text":"hi"}]},
                {"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]},
                {"role":"user","parts":[
                    {"functionResponse":{"name":"f","response":{"r":1}}}
                ]}
            ]
        }"#,
        )
        .unwrap();
        // Last content is user but pure functionResponse → not a fresh turn.
        assert_eq!(is_user_turn_start(&req), Some(false));

        let req_fresh: Value = serde_json::from_str(
            r#"{
            "contents": [
                {"role":"user","parts":[{"text":"please help"}]}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(is_user_turn_start(&req_fresh), Some(true));
    }

    #[test]
    fn test_extract_assistant_text_strips_thought() {
        let resp: Value = serde_json::from_str(
            r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"thought": true, "text": "internal monologue"},
                        {"text": "visible answer"},
                        {"functionCall": {"name": "f", "args": {}}}
                    ]
                }
            }]
        }"#,
        )
        .unwrap();
        assert_eq!(
            extract_assistant_text_value(&resp).as_deref(),
            Some("visible answer")
        );
    }

    #[test]
    fn test_sse_pcap_fixture_parses() {
        // End-to-end fixture extracted from a real Gemini-CLI API-key pcap.
        // Validates the full pipeline against bytes the proxy actually emitted.
        let api = GeminiAiStudioWireApi;
        let raw = include_str!("../../tests/fixtures/gemini-aistudio/response-streaming.sse");
        let events: Vec<SseEventData> = raw
            .split("\n\n")
            .filter_map(|chunk| {
                let trimmed = chunk.trim();
                let stripped = trimmed.strip_prefix("data:")?.trim();
                if stripped.is_empty() {
                    return None;
                }
                Some(make_sse(stripped))
            })
            .collect();
        assert!(events.len() > 10, "fixture should have many chunks");
        let (info, cache) = api.extract_sse(&events);
        // Real fixture: STOP + functionCalls present ⇒ TOOL_USE synthesized.
        assert_eq!(info.finish_reason.as_deref(), Some(SYNTHETIC_TOOL_USE));
        assert!(info.input_tokens.is_some());
        assert!(info.output_tokens.is_some());
        let body = cache.get().expect("synthetic body");
        let parts = body
            .pointer("/candidates/0/content/parts")
            .and_then(|v| v.as_array())
            .expect("parts array");
        let function_calls = parts
            .iter()
            .filter(|p| p.get("functionCall").is_some())
            .count();
        assert!(function_calls >= 1, "fixture has at least one functionCall");
    }

    // ─── first_user_text: strict "first role:user" semantics ────────────────

    #[test]
    fn first_user_text_none_when_first_user_has_no_text() {
        // First user content has only `functionResponse` parts (no text);
        // a later user has text. Strict semantics: don't fall through.
        let req = serde_json::json!({
            "contents": [
                {"role":"user","parts":[
                    {"functionResponse":{"name":"f","response":{"r":1}}}
                ]},
                {"role":"model","parts":[{"text":"ack"}]},
                {"role":"user","parts":[{"text":"later"}]}
            ]
        });
        assert_eq!(first_user_text(&req), None);
    }

    #[test]
    fn first_user_text_none_when_first_user_text_is_whitespace() {
        let req = serde_json::json!({
            "contents": [
                {"role":"user","parts":[{"text":"   "}]},
                {"role":"model","parts":[{"text":"ack"}]},
                {"role":"user","parts":[{"text":"later"}]}
            ]
        });
        assert_eq!(first_user_text(&req), None);
    }

    // ─── first_assistant_sig_from_request: strict "first only" semantics ────

    #[test]
    fn first_assistant_sig_from_request_returns_first_models_sig() {
        let req = serde_json::json!({
            "contents": [
                {"role":"user","parts":[{"text":"u1"}]},
                {"role":"model","parts":[{"text":"first model reply"}]},
                {"role":"user","parts":[{"text":"u2"}]},
                {"role":"model","parts":[{"functionCall":{"name":"f","args":{"x":1}}}]}
            ]
        });
        let sig = first_assistant_sig_from_request(&req).unwrap();
        // First model is text-only → wrap_sig produces AssistantSig::Text.
        assert!(matches!(sig, AssistantSig::Text(t) if t.contains("first model reply")));
    }

    #[test]
    fn first_assistant_sig_from_request_none_when_first_model_parts_all_thought() {
        // First model's parts are all `thought:true` (chain-of-thought
        // tokens that parts_sig deliberately filters out). Must NOT fall
        // through to the second model content.
        let req = serde_json::json!({
            "contents": [
                {"role":"user","parts":[{"text":"u1"}]},
                {"role":"model","parts":[
                    {"thought":true,"text":"hidden cot"}
                ]},
                {"role":"user","parts":[{"text":"u2"}]},
                {"role":"model","parts":[{"text":"this would be a leak"}]}
            ]
        });
        assert!(first_assistant_sig_from_request(&req).is_none());
    }

    #[test]
    fn first_assistant_sig_from_request_none_when_first_model_parts_empty() {
        let req = serde_json::json!({
            "contents": [
                {"role":"user","parts":[{"text":"u1"}]},
                {"role":"model","parts":[]},
                {"role":"user","parts":[{"text":"u2"}]},
                {"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]}
            ]
        });
        assert!(first_assistant_sig_from_request(&req).is_none());
    }
}
