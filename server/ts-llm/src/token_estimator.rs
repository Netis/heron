//! Fallback token estimator for LLM calls whose response payload omits the
//! `usage` field (LiteLLM proxy + assorted self-hosted backends do this).
//!
//! Built around the `cl100k_base` BPE table from `tiktoken-rs`. cl100k is the
//! tokenizer for OpenAI's GPT-4 / GPT-4o / GPT-3.5 family; for non-OpenAI
//! models proxied through OpenAI-shaped APIs (Qwen / GLM / DeepSeek behind
//! LiteLLM) the count drifts roughly +/-15-25%. For Anthropic the drift is
//! similar — Anthropic's tokenizer is not public. The number is meant to be
//! "good enough for capacity planning and rough cost estimates"; never
//! authoritative.
//!
//! Estimates produced by this module are always rounded to `u32`. Zero-text
//! inputs return zero. Concurrency: `CL100kEstimator` is `Send + Sync`; load
//! it once per process.
//!
//! # Reasoning-token compatibility
//!
//! Reasoning text is emitted by different servers under different field names
//! and shapes; the estimator must count it once regardless of channel:
//!
//! * `message.reasoning_content` — DeepSeek-R1 / Qwen3 native shape.
//! * `message.reasoning` — vLLM 0.17+ rename on the patched B300 image.
//!   Servers emit one OR the other; some emit both with identical payload.
//! * `<think>...</think>` blocks embedded inside `message.content` when
//!   `--reasoning-parser` was off on the server.
//! * Anthropic `content[*].type == "thinking"` extended-thinking blocks.
//! * OpenAI Responses `output[*].type == "reasoning"` blocks (with
//!   `summary[*].text` / inner `content[*].text`).
//!
//! `collect_chat_assistant_text` handles the OpenAI Chat shape with
//! deduplication by exact-string equality. The Anthropic / Responses shapes
//! are walked by their respective wire-api walkers, but both delegate the
//! `<think>...</think>` extraction back here via `extract_think_blocks`.

use std::sync::Arc;

use regex::Regex;
use serde_json::Value;
use tiktoken_rs::{cl100k_base, CoreBPE};

/// Counts tokens in arbitrary text. Implementations must be deterministic and
/// thread-safe.
pub trait TokenEstimator: Send + Sync {
    fn count_text(&self, text: &str) -> u32;
}

/// Production estimator using `tiktoken-rs::cl100k_base`. Construction loads
/// the bundled BPE table (~4MB binary). Construct once per process and share.
pub struct CL100kEstimator {
    bpe: CoreBPE,
}

impl CL100kEstimator {
    pub fn new() -> Self {
        Self {
            bpe: cl100k_base().expect("tiktoken-rs cl100k_base bundled BPE must load"),
        }
    }

    pub fn shared() -> Arc<dyn TokenEstimator> {
        Arc::new(Self::new())
    }
}

impl Default for CL100kEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenEstimator for CL100kEstimator {
    fn count_text(&self, text: &str) -> u32 {
        if text.is_empty() {
            return 0;
        }
        // `encode_with_special_tokens` is the tiktoken behavior used by the
        // OpenAI client libraries when computing prompt token budget; it
        // matches what the server would have counted (modulo cross-model
        // drift documented at module level).
        self.bpe.encode_with_special_tokens(text).len() as u32
    }
}

/// Strip every `<think>...</think>` block from `content` and return them as a
/// `Vec<String>`. Returns the cleaned content as the second tuple element.
/// Multiline blocks are supported. The returned think-block strings are the
/// inner contents — without the surrounding tags.
pub fn extract_think_blocks(content: &str) -> (Vec<String>, String) {
    // Compile once. The regex is small; `OnceLock` would micro-optimize but
    // adds noise. `(?s)` makes `.` match newlines.
    static PATTERN: &str = r"(?s)<think>(.*?)</think>";
    let re = Regex::new(PATTERN).expect("static think regex");
    let mut blocks: Vec<String> = Vec::new();
    let mut cleaned = String::with_capacity(content.len());
    let mut last_end = 0;
    for caps in re.captures_iter(content) {
        let whole = caps.get(0).expect("regex capture 0");
        let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        cleaned.push_str(&content[last_end..whole.start()]);
        blocks.push(inner.to_string());
        last_end = whole.end();
    }
    cleaned.push_str(&content[last_end..]);
    (blocks, cleaned)
}

fn push_unique(frags: &mut Vec<String>, s: String) {
    if !s.is_empty() && !frags.iter().any(|f| f == &s) {
        frags.push(s);
    }
}

/// Concatenate every distinct piece of assistant-emitted text on an
/// OpenAI-Chat-shaped `message` object: reasoning_content, reasoning,
/// content (with `<think>` blocks separated and stripped), and serialized
/// tool_calls. Deduplicates by exact-string equality so the same trace
/// emitted via both `reasoning_content` and `reasoning` is counted once.
pub fn collect_chat_assistant_text(message: &Value) -> String {
    let mut frags: Vec<String> = Vec::new();

    if let Some(s) = message.get("reasoning_content").and_then(|v| v.as_str()) {
        push_unique(&mut frags, s.to_string());
    }
    if let Some(s) = message.get("reasoning").and_then(|v| v.as_str()) {
        push_unique(&mut frags, s.to_string());
    }
    if let Some(c) = message.get("content").and_then(|v| v.as_str()) {
        let (think_blocks, rest) = extract_think_blocks(c);
        for tb in think_blocks {
            push_unique(&mut frags, tb);
        }
        push_unique(&mut frags, rest);
    } else if let Some(arr) = message.get("content").and_then(|v| v.as_array()) {
        // OpenAI multipart content (text + image_url etc.). Pull only text parts.
        for part in arr {
            if let Some(kind) = part.get("type").and_then(|v| v.as_str()) {
                if kind == "text" || kind == "input_text" || kind == "output_text" {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                        let (tb, rest) = extract_think_blocks(t);
                        for b in tb {
                            push_unique(&mut frags, b);
                        }
                        push_unique(&mut frags, rest);
                    }
                }
            }
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            if let Ok(s) = serde_json::to_string(tc) {
                push_unique(&mut frags, s);
            }
        }
    }

    frags.join("\n")
}

/// Concatenate distinct text on an Anthropic-shaped assistant message. The
/// Anthropic shape is `content: [{type: "text"|"thinking"|"tool_use", ...}]`.
/// `thinking` blocks carry their text under `.thinking`; `text` under `.text`;
/// `tool_use` is JSON-serialized whole. `<think>` substrings inside text
/// blocks are also extracted (defensive — Anthropic shouldn't emit them but
/// some bridged proxies do).
pub fn collect_anthropic_assistant_text(message: &Value) -> String {
    let mut frags: Vec<String> = Vec::new();

    let blocks = message.get("content").and_then(|v| v.as_array());
    if let Some(blocks) = blocks {
        for blk in blocks {
            let kind = blk.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "thinking" => {
                    if let Some(t) = blk.get("thinking").and_then(|v| v.as_str()) {
                        push_unique(&mut frags, t.to_string());
                    }
                }
                "text" => {
                    if let Some(t) = blk.get("text").and_then(|v| v.as_str()) {
                        let (tb, rest) = extract_think_blocks(t);
                        for b in tb {
                            push_unique(&mut frags, b);
                        }
                        push_unique(&mut frags, rest);
                    }
                }
                "tool_use" => {
                    if let Ok(s) = serde_json::to_string(blk) {
                        push_unique(&mut frags, s);
                    }
                }
                _ => {}
            }
        }
    } else if let Some(s) = message.get("content").and_then(|v| v.as_str()) {
        let (tb, rest) = extract_think_blocks(s);
        for b in tb {
            push_unique(&mut frags, b);
        }
        push_unique(&mut frags, rest);
    }

    frags.join("\n")
}

/// Concatenate distinct text on an OpenAI Responses-shaped output. The
/// Responses shape is `output: [{type: "message"|"reasoning"|...}]`. A
/// `reasoning` item may carry `summary[*].text` and/or inner `content[*].text`.
/// A `message` item carries `content: [{type: "output_text", text}]`.
pub fn collect_responses_output_text(output: &Value) -> String {
    let mut frags: Vec<String> = Vec::new();

    let items = match output.as_array() {
        Some(a) => a,
        None => return String::new(),
    };

    for item in items {
        let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "reasoning" => {
                if let Some(summary) = item.get("summary").and_then(|v| v.as_array()) {
                    for s in summary {
                        if let Some(t) = s.get("text").and_then(|v| v.as_str()) {
                            push_unique(&mut frags, t.to_string());
                        }
                    }
                }
                if let Some(inner) = item.get("content").and_then(|v| v.as_array()) {
                    for c in inner {
                        if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                            push_unique(&mut frags, t.to_string());
                        }
                    }
                }
            }
            "message" => {
                if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                    for c in content {
                        let ck = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if ck == "output_text" || ck == "input_text" || ck == "text" {
                            if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                                let (tb, rest) = extract_think_blocks(t);
                                for b in tb {
                                    push_unique(&mut frags, b);
                                }
                                push_unique(&mut frags, rest);
                            }
                        }
                    }
                }
            }
            "function_call" | "tool_call" => {
                if let Ok(s) = serde_json::to_string(item) {
                    push_unique(&mut frags, s);
                }
            }
            _ => {}
        }
    }

    frags.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn est() -> CL100kEstimator {
        CL100kEstimator::new()
    }

    #[test]
    fn count_text_zero_on_empty() {
        assert_eq!(est().count_text(""), 0);
    }

    #[test]
    fn count_text_positive_and_stable() {
        let e = est();
        let a = e.count_text("Hello world");
        let b = e.count_text("Hello world");
        assert!(a > 0);
        assert_eq!(a, b);
    }

    #[test]
    fn count_text_in_known_range() {
        // "Hello world" tokenizes to 2 in cl100k_base.
        assert_eq!(est().count_text("Hello world"), 2);
    }

    #[test]
    fn extract_think_blocks_single() {
        let (blocks, rest) = extract_think_blocks("<think>plan</think>answer");
        assert_eq!(blocks, vec!["plan".to_string()]);
        assert_eq!(rest, "answer");
    }

    #[test]
    fn extract_think_blocks_multiple_and_multiline() {
        let s = "<think>step\n1</think>mid<think>step\n2</think>end";
        let (blocks, rest) = extract_think_blocks(s);
        assert_eq!(blocks, vec!["step\n1".to_string(), "step\n2".to_string()]);
        assert_eq!(rest, "midend");
    }

    #[test]
    fn extract_think_blocks_none() {
        let (blocks, rest) = extract_think_blocks("plain answer");
        assert!(blocks.is_empty());
        assert_eq!(rest, "plain answer");
    }

    #[test]
    fn chat_collect_reasoning_content_only() {
        let m = json!({"role":"assistant","reasoning_content":"thoughts","content":""});
        let text = collect_chat_assistant_text(&m);
        assert!(text.contains("thoughts"));
    }

    #[test]
    fn chat_collect_reasoning_field_only() {
        // vLLM 0.17+ rename: payload arrives under `reasoning` not `reasoning_content`.
        let m = json!({"role":"assistant","reasoning":"thoughts","content":""});
        let text = collect_chat_assistant_text(&m);
        assert!(text.contains("thoughts"));
    }

    #[test]
    fn chat_collect_dedupes_reasoning_content_eq_reasoning() {
        // Server emits the SAME trace under both keys (some bridged proxies do).
        let m = json!({
            "role":"assistant",
            "reasoning_content":"long trace here",
            "reasoning":"long trace here",
            "content":"final answer"
        });
        let text = collect_chat_assistant_text(&m);
        // "long trace here" must appear exactly once.
        assert_eq!(text.matches("long trace here").count(), 1);
        assert!(text.contains("final answer"));
    }

    #[test]
    fn chat_collect_extracts_think_block_from_content() {
        let m = json!({
            "role":"assistant",
            "content":"<think>Let me reason</think>The answer is 42."
        });
        let text = collect_chat_assistant_text(&m);
        assert!(text.contains("Let me reason"));
        assert!(text.contains("The answer is 42."));
        // Tag itself must be stripped.
        assert!(!text.contains("<think>"));
    }

    #[test]
    fn chat_collect_dedupes_think_against_reasoning_field() {
        let m = json!({
            "role":"assistant",
            "reasoning":"Let me reason",
            "content":"<think>Let me reason</think>final"
        });
        let text = collect_chat_assistant_text(&m);
        assert_eq!(text.matches("Let me reason").count(), 1);
    }

    #[test]
    fn chat_collect_includes_tool_calls() {
        let m = json!({
            "role":"assistant",
            "content":null,
            "tool_calls":[{
                "id":"call_abc",
                "type":"function",
                "function":{"name":"do_thing","arguments":"{\"k\":\"v\"}"}
            }]
        });
        let text = collect_chat_assistant_text(&m);
        assert!(text.contains("do_thing"));
        assert!(text.contains("call_abc"));
    }

    #[test]
    fn chat_collect_multipart_content() {
        let m = json!({
            "role":"assistant",
            "content":[
                {"type":"text","text":"<think>x</think>hello"}
            ]
        });
        let text = collect_chat_assistant_text(&m);
        assert!(text.contains("x"));
        assert!(text.contains("hello"));
        assert!(!text.contains("<think>"));
    }

    #[test]
    fn anthropic_collect_thinking_and_text() {
        let m = json!({
            "role":"assistant",
            "content":[
                {"type":"thinking","thinking":"step by step"},
                {"type":"text","text":"final answer"}
            ]
        });
        let text = collect_anthropic_assistant_text(&m);
        assert!(text.contains("step by step"));
        assert!(text.contains("final answer"));
    }

    #[test]
    fn anthropic_collect_thinking_dedup_against_text_think_block() {
        // Defensive: if a bridged proxy double-emits the trace as both a
        // thinking block AND inline `<think>` inside a text block, count once.
        let m = json!({
            "role":"assistant",
            "content":[
                {"type":"thinking","thinking":"trace"},
                {"type":"text","text":"<think>trace</think>answer"}
            ]
        });
        let text = collect_anthropic_assistant_text(&m);
        assert_eq!(text.matches("trace").count(), 1);
        assert!(text.contains("answer"));
    }

    #[test]
    fn anthropic_collect_tool_use() {
        let m = json!({
            "role":"assistant",
            "content":[
                {"type":"tool_use","id":"toolu_x","name":"foo","input":{"q":"v"}}
            ]
        });
        let text = collect_anthropic_assistant_text(&m);
        assert!(text.contains("toolu_x"));
        assert!(text.contains("foo"));
    }

    #[test]
    fn responses_collect_reasoning_summary_and_message() {
        let output = json!([
            {"type":"reasoning","summary":[{"type":"summary_text","text":"sum"}],
             "content":[{"type":"reasoning_text","text":"deep"}]},
            {"type":"message","content":[{"type":"output_text","text":"answer"}]}
        ]);
        let text = collect_responses_output_text(&output);
        assert!(text.contains("sum"));
        assert!(text.contains("deep"));
        assert!(text.contains("answer"));
    }

    #[test]
    fn responses_collect_dedupes_repeated_summary() {
        let output = json!([
            {"type":"reasoning","summary":[
                {"text":"trace"},
                {"text":"trace"}
            ]},
            {"type":"message","content":[{"type":"output_text","text":"ok"}]}
        ]);
        let text = collect_responses_output_text(&output);
        assert_eq!(text.matches("trace").count(), 1);
    }

    #[test]
    fn responses_collect_function_call() {
        let output = json!([
            {"type":"function_call","name":"do_it","arguments":"{}","call_id":"call_xyz"}
        ]);
        let text = collect_responses_output_text(&output);
        assert!(text.contains("do_it"));
        assert!(text.contains("call_xyz"));
    }

    #[test]
    fn count_text_through_estimator_on_collected_text() {
        let m = json!({
            "role":"assistant",
            "reasoning_content":"some thinking",
            "content":"final"
        });
        let text = collect_chat_assistant_text(&m);
        let n = est().count_text(&text);
        assert!(n > 0);
    }
}
