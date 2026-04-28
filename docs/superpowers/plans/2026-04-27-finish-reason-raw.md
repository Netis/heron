# `finish_reason` Raw-First Refactor — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop normalizing `stop_reason`/`finish_reason` at the parsing layer. Persist each provider's raw string verbatim, simplify `TurnStatus` to `Complete | Incomplete`, move the 5-bucket dashboard counters to a long-format `llm_finish_metrics` table, and remove the now-misleading "Errors only" filter.

**Architecture:** Replace `FinishReason` enum with `Option<String>` on `LlmCall`. Push terminal/tool-use semantics into the `WireApi` trait as `is_terminal(&str) -> bool` and `is_tool_use(&str) -> bool`. Tracker uses these predicates and only distinguishes `Complete` (terminal landed) vs `Incomplete` (no terminal). Metrics splits the `finish_*_count` columns out of the wide `llm_metrics` table into a long `llm_finish_metrics(time, …, finish_reason, count)` table so unbounded raw values fit. Frontend removes "Errors only", redesigns the finish-reason filter as a wire-api-grouped dropdown, and rewrites the traffic chart to read the long table.

**Tech Stack:** Rust (Tokio, Axum, DuckDB), TypeScript (React, Vite, TanStack Query, Tailwind).

**Affected wire APIs:** `anthropic`, `openai-chat`, `openai-responses` (3 total — confirmed via `server/ts-llm/src/wire_apis/`).

**Storage backends in scope:** DuckDB (`server/ts-storage/src/duckdb.rs`). PG/CH backends per `CLAUDE.md` are pluggable but not currently shipped — if files exist they must be updated in the same phase; if they don't, no action.

**Reference docs:** [Anthropic stop_reason guide](https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons) — 7 values: `end_turn`, `stop_sequence`, `max_tokens`, `tool_use`, `pause_turn`, `refusal`, `model_context_window_exceeded`.

---

## File Structure

### Created

| File | Responsibility |
|---|---|
| `server/ts-llm/src/wire_apis/predicates_test.rs` (or inline `#[cfg(test)] mod`) | Tests for `is_terminal` / `is_tool_use` per wire API |
| `server/ts-storage/src/migrations/2026_04_27_finish_metrics_split.sql` (or inline string in `duckdb.rs`) | Drop 5 finish_* columns from `llm_metrics`; create `llm_finish_metrics` long table |
| `console/src/lib/finish-tone.ts` | Frontend tone map: `(rawReason) -> "ok"\|"warn"\|"tool"\|"pause"\|"err"` |

### Modified

| File | Change |
|---|---|
| `server/ts-llm/src/model.rs` | Delete `FinishReason` enum and `Display`; `LlmCall.finish_reason: Option<String>` |
| `server/ts-llm/src/wire_apis/mod.rs` | Add `is_terminal` and `is_tool_use` to `WireApi` trait |
| `server/ts-llm/src/wire_apis/anthropic.rs` | Remove `map_stop_reason`; impl trait predicates; raw extraction |
| `server/ts-llm/src/wire_apis/openai/chat.rs` | Remove finish_reason mapping; impl trait predicates; raw extraction |
| `server/ts-llm/src/wire_apis/openai/responses.rs` | Same |
| `server/ts-llm/src/processor.rs` | `ResponseInfo.finish_reason: Option<String>` |
| `server/ts-llm/src/profile.rs` | Remove `FinishReason` import; any references switch to string |
| `server/ts-llm/src/stage.rs` | Test fixtures — switch to raw values |
| `server/ts-turn/src/model.rs` | `TurnStatus` collapses to `Complete \| Incomplete` |
| `server/ts-turn/src/tracker.rs` | `is_main_terminal` calls `wire_api.is_terminal`; status derivation simplified |
| `server/ts-turn/src/stage.rs` | Test fixtures — raw values |
| `server/ts-turn/tests/reorder.rs` | Test fixtures — raw values |
| `server/ts-metrics/src/bucket.rs` | Replace 5 fixed counters with `BTreeMap<String, u64>` keyed by raw `finish_reason` |
| `server/ts-metrics/src/aggregator.rs` | Emit per-key counts in finalize |
| `server/ts-metrics/src/stage.rs` | Test fixtures |
| `server/ts-storage/src/backend.rs` | New `LlmFinishMetric` struct; updated `LlmMetric` (no finish_* fields) |
| `server/ts-storage/src/duckdb.rs` | Drop finish_* columns; create `llm_finish_metrics`; new write/read paths; update tests |
| `server/ts-storage/src/buffer.rs` / `sink.rs` | Buffer + sink the new long-format rows |
| `server/ts-storage/src/query.rs` | Remove `errors_only`; add long-table query types |
| `server/ts-api/src/routes/llm_calls.rs` | Remove `errors_only` field + tests |
| `server/ts-api/src/routes/metrics.rs` (or wherever `/api/metrics` lives) | Add endpoint that reads `llm_finish_metrics` |
| `console/src/types/api.ts` | No type changes; comment update on `finish_reason` value semantics |
| `console/src/hooks/use-llm-calls.ts` | Drop `errorsOnly` |
| `console/src/pages/llm-calls.tsx` | Remove "Errors only" toggle; redesign finish-reason filter; pass `wire_api` to badge |
| `console/src/pages/traffic.tsx` | Switch finish-reason chart to long-table API; tone-based coloring |
| `console/src/components/ui/finish-badge.tsx` | colorMap covers all raw provider values via `finish-tone.ts` |
| `console/src/components/turn-detail/call-card.tsx` | Use `finishTone(...) === "err"` for visual error; drop `"truncated"` dead branch |
| `console/src/components/turn-detail/gantt-nav.tsx` | Same |
| `console/src/lib/turn-index.test.ts` | Replace `"complete"` etc. with raw provider values |
| `docs/design/07-schema.md` | Document `finish_reason` value semantics; document `llm_finish_metrics` long table |
| `CHANGELOG.md` | Breaking changes (added at the very end) |

### Deleted

None. (All changes are field-shape, not file-level.)

---

## Phase 1: Wire API trait predicates (no behavior change yet)

The new `is_terminal` / `is_tool_use` predicates can land first as additive trait methods. Existing `FinishReason` enum coexists. This unblocks Phase 3 (tracker) without touching parsing.

### Task 1.1: Add `is_terminal` / `is_tool_use` to `WireApi` trait

**Files:**
- Modify: `server/ts-llm/src/wire_apis/mod.rs` (look up actual `pub trait WireApi` location — may be in `wire_api_registry.rs`)
- Modify: `server/ts-llm/src/wire_apis/anthropic.rs`
- Modify: `server/ts-llm/src/wire_apis/openai/chat.rs`
- Modify: `server/ts-llm/src/wire_apis/openai/responses.rs`

- [ ] **Step 1: Locate the `WireApi` trait**

```bash
rg -n "pub trait WireApi" server/ts-llm/src/
```

Expected: one hit naming the file and line. Open that file.

- [ ] **Step 2: Add trait methods with default panicking impl**

In the trait definition file, add:

```rust
/// True iff `finish_reason` is a wire-level terminal — i.e. the model has
/// finished emitting this message and the agent loop must decide whether to
/// continue (e.g. tool result) or finalize. Anthropic `pause_turn` is NOT
/// terminal: the assistant turn continues after the server-tool loop yields.
fn is_terminal(&self, finish_reason: &str) -> bool;

/// True iff `finish_reason` indicates the model is requesting tool execution
/// and expects a tool_result message in the next turn.
fn is_tool_use(&self, finish_reason: &str) -> bool;
```

- [ ] **Step 3: Run cargo check to see breakage**

Run: `cargo check -p ts-llm`
Expected: compile errors at all `impl WireApi for ...` sites (3 sites).

- [ ] **Step 4: Implement for `AnthropicWireApi`**

In `server/ts-llm/src/wire_apis/anthropic.rs`, inside `impl WireApi for AnthropicWireApi`:

```rust
fn is_terminal(&self, finish_reason: &str) -> bool {
    matches!(
        finish_reason,
        "end_turn"
            | "stop_sequence"
            | "max_tokens"
            | "tool_use"
            | "refusal"
            | "model_context_window_exceeded"
    )
    // pause_turn intentionally absent — server-tool loop yielded mid-turn.
}

fn is_tool_use(&self, finish_reason: &str) -> bool {
    finish_reason == "tool_use"
}
```

- [ ] **Step 5: Implement for `OpenAiChatWireApi`**

In `server/ts-llm/src/wire_apis/openai/chat.rs`:

```rust
fn is_terminal(&self, finish_reason: &str) -> bool {
    matches!(
        finish_reason,
        "stop" | "length" | "tool_calls" | "function_call" | "content_filter"
    )
}

fn is_tool_use(&self, finish_reason: &str) -> bool {
    matches!(finish_reason, "tool_calls" | "function_call")
}
```

- [ ] **Step 6: Implement for `OpenAiResponsesWireApi`**

In `server/ts-llm/src/wire_apis/openai/responses.rs`:

```rust
fn is_terminal(&self, finish_reason: &str) -> bool {
    matches!(finish_reason, "completed" | "incomplete" | "failed" | "cancelled")
}

fn is_tool_use(&self, finish_reason: &str) -> bool {
    // Responses API surfaces tool use via output items, not finish_reason.
    // Keep predicate false; tracker should rely on output-item inspection
    // (existing code path).
    false
}
```

- [ ] **Step 7: Run cargo check**

Run: `cargo check -p ts-llm`
Expected: clean.

- [ ] **Step 8: Add unit tests**

Append to each `wire_apis/*.rs` test module:

```rust
#[test]
fn predicates_anthropic() {
    let w = AnthropicWireApi;
    assert!(w.is_terminal("end_turn"));
    assert!(w.is_terminal("max_tokens"));
    assert!(w.is_terminal("refusal"));
    assert!(w.is_terminal("model_context_window_exceeded"));
    assert!(!w.is_terminal("pause_turn"));
    assert!(!w.is_terminal("unknown_future_value"));
    assert!(w.is_tool_use("tool_use"));
    assert!(!w.is_tool_use("end_turn"));
}
```

(Equivalent test in each of the other two wire_api files using their respective vocabularies.)

- [ ] **Step 9: Run tests**

Run: `cargo test -p ts-llm wire_apis::`
Expected: all green, including new predicate tests.

- [ ] **Step 10: Commit**

```bash
git add server/ts-llm/src/wire_apis/
git commit -m "feat(ts-llm): add is_terminal/is_tool_use predicates to WireApi trait"
```

---

## Phase 2: Drop `FinishReason` enum, switch to raw `Option<String>`

### Task 2.1: Change `LlmCall.finish_reason` field type

**Files:**
- Modify: `server/ts-llm/src/model.rs`

- [ ] **Step 1: Delete the enum and its Display**

In `server/ts-llm/src/model.rs`, delete lines 20-45 (the entire `FinishReason` enum + `Display` impl). Replace with nothing (no successor type).

- [ ] **Step 2: Change field type on `LlmCall`**

Around line 65, change:

```rust
pub finish_reason: Option<FinishReason>,
```

to:

```rust
/// Raw provider finish_reason (`stop_reason` for Anthropic, `finish_reason`
/// for OpenAI Chat, etc.). Verbatim string from the wire — no normalization.
/// Use the owning `wire_api` to interpret.
pub finish_reason: Option<String>,
```

- [ ] **Step 3: cargo check to see all break sites**

Run: `cargo check --workspace 2>&1 | head -100`
Expected: many errors referencing `FinishReason`. **Save the list** — these drive Tasks 2.2–2.6.

- [ ] **Step 4: Commit (compile broken — that's expected; fixed in next tasks)**

DO NOT commit yet. Wait until Task 2.6.

### Task 2.2: Update Anthropic extractor

**Files:**
- Modify: `server/ts-llm/src/wire_apis/anthropic.rs`

- [ ] **Step 1: Delete `map_stop_reason`**

Delete the function at lines 437-446 plus its `test_map_stop_reason` test (lines 473-478).

- [ ] **Step 2: Change non-streaming extraction**

At lines 140-143, replace:

```rust
let finish_reason = body
    .get("stop_reason")
    .and_then(|v| v.as_str())
    .map(map_stop_reason);
```

with:

```rust
let finish_reason = body
    .get("stop_reason")
    .and_then(|v| v.as_str())
    .map(|s| s.to_string());
```

- [ ] **Step 3: Change streaming extraction**

At line 233-234, replace:

```rust
if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
    finish_reason = Some(map_stop_reason(sr));
}
```

with:

```rust
if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
    finish_reason = Some(sr.to_string());
}
```

The local `finish_reason: Option<FinishReason>` declaration (line 185) needs to become `Option<String>` — change it.

- [ ] **Step 4: Drop `FinishReason` import**

Remove `FinishReason` from any `use` statement at the top of the file. The struct/enum no longer exists.

- [ ] **Step 5: Update existing tests**

Find any test that asserts `assert_eq!(call.finish_reason, Some(FinishReason::ToolUse))` and rewrite as `assert_eq!(call.finish_reason.as_deref(), Some("tool_use"))`.

```bash
rg -n "FinishReason::" server/ts-llm/src/wire_apis/anthropic.rs
```

Update each match.

- [ ] **Step 6: cargo check this file**

Run: `cargo check -p ts-llm`
Expected: errors only outside this file now.

### Task 2.3: Update OpenAI Chat extractor

**Files:**
- Modify: `server/ts-llm/src/wire_apis/openai/chat.rs`

- [ ] **Step 1: Find and delete the mapping function**

```bash
rg -n "fn map_finish|FinishReason::" server/ts-llm/src/wire_apis/openai/chat.rs
```

Identify the mapping function (likely `map_finish_reason` or inline match on lines 320-330 area). Delete it including the `content_filter => Cancelled` line (323).

- [ ] **Step 2: Replace with raw string extraction**

Wherever the deleted function was called, replace with `.map(|s| s.to_string())` directly on the JSON string accessor.

- [ ] **Step 3: Drop import + update tests**

Remove `FinishReason` from `use`. Rewrite test assertions to compare strings.

- [ ] **Step 4: cargo check**

Run: `cargo check -p ts-llm`
Expected: progress (errors remain in other files but not this one).

### Task 2.4: Update OpenAI Responses extractor

**Files:**
- Modify: `server/ts-llm/src/wire_apis/openai/responses.rs`

- [ ] **Step 1: Same surgery as Task 2.3**

Find mapping at line 239 area (`"cancelled" => FinishReason::Cancelled`). Delete the mapping function. Replace call sites with raw `.map(|s| s.to_string())`.

- [ ] **Step 2: Update tests, drop import, cargo check**

Run: `cargo check -p ts-llm`
Expected: ts-llm clean.

### Task 2.5: Update `processor.rs`, `profile.rs`, `stage.rs`

**Files:**
- Modify: `server/ts-llm/src/processor.rs`
- Modify: `server/ts-llm/src/profile.rs`
- Modify: `server/ts-llm/src/stage.rs`

- [ ] **Step 1: processor.rs `ResponseInfo` field**

Find:

```bash
rg -n "finish_reason" server/ts-llm/src/processor.rs
```

Change `pub finish_reason: Option<FinishReason>` to `pub finish_reason: Option<String>`. Update test fixtures (around lines 168-326): assertions like `Some(FinishReason::Complete)` become `Some("stop".to_string())` (for OpenAI fixtures) or `Some("end_turn".to_string())` (for Anthropic fixtures).

- [ ] **Step 2: profile.rs**

```bash
rg -n "FinishReason" server/ts-llm/src/profile.rs
```

If only comment mentions remain, leave them but rephrase to "raw finish_reason"; if there's a code reference, replace with String.

- [ ] **Step 3: stage.rs**

Same — fixtures around lines 231 and 278 already use raw JSON strings (`"stop"`, `"end_turn"`); the only change is the assertion target type (`Option<String>`).

- [ ] **Step 4: cargo check**

Run: `cargo check -p ts-llm`
Expected: clean.

### Task 2.6: Update consumers in `ts-metrics` and `ts-turn` (compile-only)

**Files:**
- Modify: `server/ts-metrics/src/bucket.rs`
- Modify: `server/ts-metrics/src/aggregator.rs`
- Modify: `server/ts-metrics/src/stage.rs`
- Modify: `server/ts-turn/src/tracker.rs`
- Modify: `server/ts-turn/src/stage.rs`
- Modify: `server/ts-turn/tests/reorder.rs`

This task is **stub-only** — get the workspace compiling so we can run tests. Phase 3 + 4 give these files their final shape.

- [ ] **Step 1: bucket.rs — temporary stub**

At lines 194-201, replace the match on `FinishReason::*` with:

```rust
if let Some(reason) = call.finish_reason.as_deref() {
    *self.finish_counts.entry(reason.to_string()).or_insert(0) += 1;
}
```

Add to the struct:

```rust
pub finish_counts: std::collections::BTreeMap<String, u64>,
```

Remove the five `finish_*_count: u64` fields. Remove the `FinishReason` import.

- [ ] **Step 2: aggregator.rs / stage.rs**

Replace `FinishReason::Complete` etc. in fixtures with raw strings: `Some("stop".to_string())`, `Some("end_turn".to_string())`, etc. Choose values consistent with each fixture's `wire_api`.

- [ ] **Step 3: tracker.rs — temporary stub**

At lines 605-606 and 712-718, **temporarily** stub:

```rust
// In is_main_terminal:
matches!(ic.call.finish_reason.as_deref(), Some(_))  // TEMP: refined in Phase 3
```

```rust
// status derivation:
let status = match terminal {
    Some(_) => TurnStatus::Complete,  // TEMP: refined in Phase 3
    None => TurnStatus::Incomplete,
};
```

These stubs are wrong (will eat tool_use as terminal); Phase 3 fixes them. Tag with `// TODO(phase3)`.

- [ ] **Step 4: turn fixtures**

`stage.rs` (line 135) and `reorder.rs` (line 58, 91, 505) — change `finish: FinishReason` parameter type to `finish: &str` and update call sites accordingly:
- `FinishReason::Complete` → `"end_turn"` (Anthropic fixtures) or `"stop"` (OpenAI)
- `FinishReason::ToolUse` → `"tool_use"` or `"tool_calls"`
- `FinishReason::Length` → `"max_tokens"` or `"length"`
- `FinishReason::Cancelled` → `"cancelled"` (OpenAI Responses) or pick the appropriate raw value

Decide per fixture based on the `wire_api` it constructs.

- [ ] **Step 5: cargo build --workspace**

Run: `cargo build --workspace 2>&1 | tail -40`
Expected: clean build (warnings about TODOs OK).

- [ ] **Step 6: cargo test --workspace (expect some failures)**

Run: `cargo test --workspace 2>&1 | tail -40`
Expected: turn-tracker tests will fail because of the stubbed `is_main_terminal` (treating tool_use as terminal closes turns prematurely). Other tests should mostly pass. **Note** the failures — Phase 3 fixes them.

- [ ] **Step 7: Commit**

```bash
git add server/
git commit -m "refactor(ts-llm): drop FinishReason enum, store raw provider strings

- LlmCall.finish_reason: Option<FinishReason> -> Option<String>
- Wire-API extractors return raw stop_reason/finish_reason verbatim
- Bucket counters keyed by raw string (BTreeMap)
- Tracker terminal logic stubbed; refined in next commit"
```

---

## Phase 3: Tracker — collapse `TurnStatus`, route through trait predicates

### Task 3.1: Simplify `TurnStatus`

**Files:**
- Modify: `server/ts-turn/src/model.rs`

- [ ] **Step 1: Replace enum**

At lines 16-28, replace the 5-variant enum with:

```rust
/// Whether this turn closed cleanly. The wire-level reason (e.g. `end_turn`,
/// `max_tokens`, `refusal`) lives in `final_finish_reason: Option<String>` —
/// status only encodes "did a terminal land before finalize". `Incomplete`
/// means we never saw a wire-level terminal: idle timeout, pcap EOF, server
/// shutdown, or connection RST mid-stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Complete,
    Incomplete,
}

impl fmt::Display for TurnStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TurnStatus::Complete => write!(f, "complete"),
            TurnStatus::Incomplete => write!(f, "incomplete"),
        }
    }
}
```

- [ ] **Step 2: Update model tests**

Around lines 128-129 the existing `TurnStatus::Complete.to_string()` test stays. Delete any test asserting on `Length`/`Cancelled`/`Failed` variants.

- [ ] **Step 3: cargo check**

Run: `cargo check -p ts-turn`
Expected: errors at any `match TurnStatus { ... Length => ... }` or `TurnStatus::Failed` reference. Note them.

### Task 3.2: Rewrite tracker terminal + status derivation

**Files:**
- Modify: `server/ts-turn/src/tracker.rs`

- [ ] **Step 1: Look up the wire_api lookup mechanism**

```bash
rg -n "registry|by_name|wire_api_registry" server/ts-turn/src/ server/ts-llm/src/wire_api_registry.rs | head -20
```

Identify how `tracker.rs` already accesses wire-API state. There's likely a `WireApiRegistry` available — if not, the call's `wire_api: &'static str` is passed in and we look it up.

- [ ] **Step 2: Replace `is_main_terminal` predicate**

At lines 605-606, remove the temporary stub. New impl:

```rust
fn is_main_terminal(profile: &dyn AgentProfile, ic: &InflightCall) -> bool {
    let Some(reason) = ic.call.finish_reason.as_deref() else {
        return false;
    };
    let Some(api) = profile.wire_api_for(&ic.call) else {
        return false;
    };
    api.is_terminal(reason) && !api.is_tool_use(reason)
        && profile.is_main_agent_call(ic)
}
```

(Adjust signatures based on what `profile`/`AgentProfile` actually exposes. The point: terminal-but-tool-use means the agent loop continues; only non-tool-use terminals close the turn.)

- [ ] **Step 3: Rewrite status derivation**

At lines 712-718, replace the stubbed match with:

```rust
let status = match terminal {
    Some(_) => TurnStatus::Complete,
    None => TurnStatus::Incomplete,
};
```

The rich state moved into `final_finish_reason` (which already passes through as `String`).

- [ ] **Step 4: Run reorder tests**

Run: `cargo test -p ts-turn --test reorder 2>&1 | tail -30`
Expected: all pass. If any fail, the regression is in fixture-data or `is_main_terminal` semantic — debug there.

- [ ] **Step 5: Run all ts-turn tests**

Run: `cargo test -p ts-turn 2>&1 | tail -30`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add server/ts-turn/
git commit -m "refactor(ts-turn): collapse TurnStatus to Complete|Incomplete

Wire-level finish_reason variety now lives entirely in
final_finish_reason: Option<String>. TurnStatus only encodes whether a
terminal landed before finalize. is_main_terminal routes through the
WireApi trait (is_terminal && !is_tool_use)."
```

---

## Phase 4: Metrics — long-format `llm_finish_metrics` table

The existing wide `llm_metrics` table has 5 fixed `finish_*_count` columns. Splitting into a separate long table keeps `llm_metrics` semantics unchanged while letting finish-reason live as raw strings.

### Task 4.1: Define `LlmFinishMetric` struct

**Files:**
- Modify: `server/ts-storage/src/backend.rs`

- [ ] **Step 1: Find where `LlmMetric` is defined**

```bash
rg -n "pub struct LlmMetric" server/ts-storage/
```

- [ ] **Step 2: Add new long-row struct alongside it**

Append:

```rust
/// One row of finish-reason counts in the long-format `llm_finish_metrics`
/// table. Emitted alongside `LlmMetric` by the bucket finalizer; one row per
/// distinct raw `finish_reason` observed in a given bucket dimension.
#[derive(Debug, Clone)]
pub struct LlmFinishMetric {
    pub timestamp_us: i64,
    pub source_id: String,
    pub granularity: String,
    pub wire_api: String,
    pub model: String,
    pub server_ip: String,
    /// Raw provider value: `end_turn`, `stop`, `pause_turn`, `STOP`, etc.
    pub finish_reason: String,
    pub count: u64,
}
```

- [ ] **Step 3: Remove finish_*_count fields from `LlmMetric`**

Delete the 5 fields. Update any `Default` impl, `new` ctor, and all literal construction sites (search via `cargo check`).

- [ ] **Step 4: cargo check**

Run: `cargo check -p ts-storage`
Expected: errors at all sites that built a `LlmMetric` with the 5 fields. Most are tests and the bucket finalizer.

### Task 4.2: Bucket finalize emits both metric kinds

**Files:**
- Modify: `server/ts-metrics/src/bucket.rs`
- Modify: `server/ts-metrics/src/aggregator.rs`

- [ ] **Step 1: Find the finalize method**

```bash
rg -n "fn finalize|fn flush|to_metric" server/ts-metrics/src/bucket.rs server/ts-metrics/src/aggregator.rs
```

Identify how a `Bucket` becomes a `LlmMetric`.

- [ ] **Step 2: Change return type**

Wherever `finalize` returns `LlmMetric`, change to `(LlmMetric, Vec<LlmFinishMetric>)`. Build the vec by iterating `self.finish_counts`:

```rust
let finish_metrics: Vec<LlmFinishMetric> = self
    .finish_counts
    .iter()
    .map(|(reason, count)| LlmFinishMetric {
        timestamp_us: bucket_ts,
        source_id: self.source_id.clone(),
        granularity: granularity.to_string(),
        wire_api: self.wire_api.clone(),
        model: self.model.clone(),
        server_ip: self.server_ip.clone(),
        finish_reason: reason.clone(),
        count: *count,
    })
    .collect();
(metric, finish_metrics)
```

- [ ] **Step 3: Update aggregator orchestration**

Whichever code aggregates buckets and dispatches to storage: collect the `Vec<LlmFinishMetric>` alongside the `Vec<LlmMetric>`.

- [ ] **Step 4: Update tests**

Test fixtures that previously asserted `metric.finish_complete_count == 1` now assert against the returned `Vec<LlmFinishMetric>`:

```rust
assert!(finish_metrics.iter().any(|m| m.finish_reason == "end_turn" && m.count == 1));
```

- [ ] **Step 5: cargo test**

Run: `cargo test -p ts-metrics 2>&1 | tail -20`
Expected: all green.

### Task 4.3: Storage — schema migration + write path

**Files:**
- Modify: `server/ts-storage/src/duckdb.rs`
- Modify: `server/ts-storage/src/buffer.rs`
- Modify: `server/ts-storage/src/sink.rs`

- [ ] **Step 1: Add new CREATE TABLE constant**

Around line 188 (next to `CREATE_LLM_METRICS`), add:

```rust
const CREATE_LLM_FINISH_METRICS: &str = "
CREATE TABLE IF NOT EXISTS llm_finish_metrics (
    timestamp     TIMESTAMP NOT NULL,
    source_id     VARCHAR NOT NULL,
    granularity   VARCHAR NOT NULL,
    wire_api      VARCHAR NOT NULL,
    model         VARCHAR NOT NULL,
    server_ip     VARCHAR NOT NULL,
    finish_reason VARCHAR NOT NULL,
    count         UBIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_llm_finish_metrics_ts
    ON llm_finish_metrics (timestamp, granularity);
";
```

- [ ] **Step 2: Drop the 5 columns from `CREATE_LLM_METRICS`**

In the `CREATE_LLM_METRICS` string at lines 213-217, remove the 5 finish_* lines. Existing prod databases need migration — see Step 4.

- [ ] **Step 3: Wire up creation**

Find where `CREATE_LLM_METRICS` is executed (around line 965). Add an `execute_batch` for `CREATE_LLM_FINISH_METRICS` immediately after.

- [ ] **Step 4: Add `ALTER TABLE` migration for existing databases**

After CREATE TABLE blocks, add idempotent migration:

```rust
let _ = conn.execute_batch(
    "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_complete_count;
     ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_length_count;
     ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_tool_use_count;
     ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_error_count;
     ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_cancelled_count;",
);
```

(Verify DuckDB supports `DROP COLUMN IF EXISTS` — if not, use `try_execute_batch` or wrap in match-and-ignore.)

- [ ] **Step 5: Update appender column lists**

Around lines 539, 644: the column lists for INSERT/appender. Remove the 5 finish columns.

Around lines 1309-1340: the appender `.appender("llm_metrics")` write logic. Remove the 5 `m.finish_*_count` arguments.

Add new appender path for `llm_finish_metrics` — rows come from `Vec<LlmFinishMetric>`.

- [ ] **Step 6: Update query paths**

Lines 1447, 1495, 1568, 1672, 3562: existing `FROM llm_metrics` queries that select `finish_*_count` columns. These need to either:
- Become two queries (wide for non-finish, long for finish), then merged, OR
- Drop their finish-reason-related output and let callers query `llm_finish_metrics` separately.

For each query site, decide based on the caller. The retention path (3035-3160) doesn't read finish-reason — only delete by granularity — so just add a parallel `DELETE FROM llm_finish_metrics WHERE granularity = ? AND timestamp < ?` next to the existing one.

- [ ] **Step 7: Update buffer.rs / sink.rs**

The write buffer that batches `LlmMetric` needs a parallel buffer or merged buffer for `LlmFinishMetric`. Cleanest: add `pub finish_metrics: Vec<LlmFinishMetric>` to the existing batch struct; flush together so they share the same transactional boundary.

- [ ] **Step 8: Update test fixtures**

Many tests at lines 3489, 3525, 3562, 5257, 5438 build `LlmMetric` literals with the 5 finish counts. Remove those fields. Where a test was specifically asserting on finish counts, rewrite as a query against `llm_finish_metrics` plus `LlmFinishMetric` insertion.

- [ ] **Step 9: cargo test --workspace**

Run: `cargo test --workspace 2>&1 | tail -40`
Expected: all green.

- [ ] **Step 10: Commit**

```bash
git add server/
git commit -m "refactor(ts-storage): split finish-reason counters into long table

llm_metrics drops 5 finish_*_count columns. New llm_finish_metrics
table is long-format (timestamp, ..., finish_reason: VARCHAR, count)
keyed by raw provider value. Existing databases drop the columns via
idempotent ALTER TABLE. Bucket finalizer emits both LlmMetric and
Vec<LlmFinishMetric>."
```

---

## Phase 5: API — drop `errors_only`, expose finish-reason long table

### Task 5.1: Remove `errors_only` end-to-end

**Files:**
- Modify: `server/ts-api/src/routes/llm_calls.rs`
- Modify: `server/ts-storage/src/query.rs`
- Modify: `server/ts-storage/src/duckdb.rs` (line 1842)

- [ ] **Step 1: Remove field from API request type**

In `routes/llm_calls.rs:34`:

```rust
pub errors_only: Option<bool>,
```

Delete the line. At line 78, delete the line that copies it into `QueryParams`.

- [ ] **Step 2: Remove from QueryParams**

In `query.rs:49`, delete `pub errors_only: bool`. Update any `Default` impl or constructors.

- [ ] **Step 3: Remove WHERE branch**

In `duckdb.rs:1842` area:

```rust
if query.errors_only {
    // ... WHERE clause fragment
}
```

Delete the whole branch.

- [ ] **Step 4: Update tests**

`routes/llm_calls.rs:148` test `errors_only_and_contains_params_parse` — rename to `contains_params_parse` and remove `errors_only=true&` from the URL fixture (line 159). Drop the assertion that checked `errors_only`.

`duckdb.rs:4355, 4398` and any other test fixture setting `errors_only: false` — remove the field from struct literals.

- [ ] **Step 5: cargo test --workspace**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: green.

### Task 5.2: Add API endpoint for `llm_finish_metrics`

**Files:**
- Modify: `server/ts-api/src/routes/metrics.rs` (or `traffic.rs` — use `rg` to confirm)

- [ ] **Step 1: Locate existing metrics endpoint**

```bash
rg -n "finish_complete_count|llm_metrics" server/ts-api/
```

Identify which handler currently serves the traffic timeseries.

- [ ] **Step 2: Add new query method on backend**

In `ts-storage/src/backend.rs` `StorageBackend` trait, add:

```rust
async fn query_finish_reasons(
    &self,
    range: TimeRange,
    granularity: &str,
    filters: MetricFilters,
) -> AppResult<Vec<FinishReasonTimeseries>>;
```

Where `FinishReasonTimeseries { finish_reason: String, points: Vec<(i64, u64)> }`.

- [ ] **Step 3: Implement on DuckDB backend**

```sql
SELECT timestamp, finish_reason, SUM(count) AS c
FROM llm_finish_metrics
WHERE granularity = ?
  AND timestamp BETWEEN ? AND ?
  AND wire_api = COALESCE(?, wire_api)
  AND model = COALESCE(?, model)
GROUP BY timestamp, finish_reason
ORDER BY timestamp, finish_reason;
```

(Adapt to the existing patterns used in the file for parameter binding.)

- [ ] **Step 4: Wire up route**

Add `GET /api/metrics/finish-reasons` returning JSON `{ series: [{ finish_reason: "end_turn", points: [[ts, count], ...] }, ...] }`.

- [ ] **Step 5: Test**

Add an integration test that inserts a few `LlmFinishMetric` rows and queries the endpoint.

- [ ] **Step 6: Commit**

```bash
git add server/
git commit -m "feat(ts-api): expose llm_finish_metrics; drop errors_only filter"
```

---

## Phase 6: Frontend

### Task 6.1: Frontend tone map

**Files:**
- Create: `console/src/lib/finish-tone.ts`

- [ ] **Step 1: Write the module**

```typescript
export type FinishTone = "ok" | "warn" | "tool" | "pause" | "err" | "muted"

const TONE: Record<string, FinishTone> = {
  // Natural completion
  end_turn: "ok",
  stop: "ok",
  STOP: "ok",
  stop_sequence: "ok",
  completed: "ok",
  // Truncation
  max_tokens: "warn",
  length: "warn",
  MAX_TOKENS: "warn",
  model_context_window_exceeded: "warn",
  incomplete: "warn",
  // Tool use
  tool_use: "tool",
  tool_calls: "tool",
  function_call: "tool",
  TOOL_CALLS: "tool",
  // Server-tool yield
  pause_turn: "pause",
  // Safety / failure
  refusal: "err",
  content_filter: "err",
  SAFETY: "err",
  RECITATION: "err",
  failed: "err",
  cancelled: "err",
}

export function finishTone(reason: string | null | undefined): FinishTone {
  if (!reason) return "muted"
  return TONE[reason] ?? "muted"
}

export const TONE_CLASS: Record<FinishTone, string> = {
  ok: "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400",
  warn: "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400",
  tool: "bg-blue-100 text-blue-700 dark:bg-blue-900/30 dark:text-blue-400",
  pause: "bg-sky-100 text-sky-700 dark:bg-sky-900/30 dark:text-sky-400",
  err: "bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-400",
  muted: "bg-gray-100 text-gray-600 dark:bg-gray-800/30 dark:text-gray-400",
}
```

- [ ] **Step 2: bun typecheck**

Run from `console/`: `bun run typecheck`
Expected: clean.

### Task 6.2: Update `FinishBadge` to use tone map

**Files:**
- Modify: `console/src/components/ui/finish-badge.tsx`

- [ ] **Step 1: Replace component body**

```tsx
import { cn } from "@/lib/utils"
import { finishTone, TONE_CLASS } from "@/lib/finish-tone"

export function FinishBadge({ reason }: { reason: string | null }) {
  if (!reason) return <span className="text-muted-foreground">—</span>
  const tone = finishTone(reason)
  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-xs font-medium",
        TONE_CLASS[tone],
      )}
    >
      {reason}
    </span>
  )
}
```

- [ ] **Step 2: bun typecheck + render check**

Run: `bun run typecheck && bun run dev` — open `/calls`, confirm badges show raw values like `end_turn`, `pause_turn`, `stop` rather than normalized labels.

### Task 6.3: Drop "Errors only" from calls page

**Files:**
- Modify: `console/src/pages/llm-calls.tsx`
- Modify: `console/src/hooks/use-llm-calls.ts`

- [ ] **Step 1: Hook**

In `hooks/use-llm-calls.ts`:
- Remove `errorsOnly?: boolean` from the params type (line 17)
- Remove `errorsOnly,` destructuring (line 29)
- Remove from query string assembly (line 39, 54)

- [ ] **Step 2: Page**

In `pages/llm-calls.tsx`:
- Delete `errorsOnlyStr`/`setErrorsOnlyStr` state (line 94)
- Delete `errorsOnly` const (line 100)
- Delete the entire `<button>...Errors only...</button>` block (lines 164-177)
- Remove `errorsOnly,` from the `useLlmCalls(...)` call (line 114)

- [ ] **Step 3: bun typecheck**

Run: `bun run typecheck`
Expected: clean.

### Task 6.4: Redesign finish-reason filter

**Files:**
- Modify: `console/src/pages/llm-calls.tsx`

- [ ] **Step 1: Replace `FINISH_OPTIONS`**

At line 14, replace:

```ts
const FINISH_OPTIONS = ["complete", "stop", "length", "tool_use", "error", "cancelled"]
```

with:

```ts
const FINISH_GROUPS = [
  {
    label: "Anthropic",
    options: ["end_turn", "stop_sequence", "max_tokens", "tool_use", "pause_turn", "refusal", "model_context_window_exceeded"],
  },
  {
    label: "OpenAI Chat",
    options: ["stop", "length", "tool_calls", "function_call", "content_filter"],
  },
  {
    label: "OpenAI Responses",
    options: ["completed", "incomplete", "failed", "cancelled"],
  },
]
```

- [ ] **Step 2: Inspect `FilterDropdown`**

```bash
rg -n "FilterDropdown" console/src/components/
```

Open the file and check whether it accepts `groups` as a prop. If not, add it: optional `groups?: { label: string; options: string[] }[]` prop alongside the existing `options` prop. When `groups` is provided, render grouped sections in the popover.

- [ ] **Step 3: Use in page**

Replace the existing `<FilterDropdown label="Finish Reason" options={FINISH_OPTIONS} ... />` with `groups={FINISH_GROUPS}`.

- [ ] **Step 4: Manual smoke**

Run: `bun run dev` — open `/calls`, click Finish Reason filter, verify grouped UI; pick `pause_turn`, confirm URL becomes `?finish=pause_turn` and that backend filter accepts it (will return rows for any wire_api whose finish_reason matches).

### Task 6.5: Call card / Gantt — use tone for visual error

**Files:**
- Modify: `console/src/components/turn-detail/call-card.tsx`
- Modify: `console/src/components/turn-detail/gantt-nav.tsx`

- [ ] **Step 1: call-card.tsx:15**

Replace:

```ts
if (call.finish_reason === "error" || call.finish_reason === "truncated") return "error"
```

with:

```ts
import { finishTone } from "@/lib/finish-tone"
// ...
const tone = finishTone(call.finish_reason)
if (tone === "err") return "error"
if (tone === "warn") return "warn"
```

(Adjust to whatever `getCallTone` / similar function this lives inside.)

- [ ] **Step 2: gantt-nav.tsx:19**

Same replacement.

- [ ] **Step 3: bun typecheck**

Run: `bun run typecheck`
Expected: clean.

### Task 6.6: Traffic page — long-table chart

**Files:**
- Modify: `console/src/pages/traffic.tsx`

- [ ] **Step 1: Replace metric names**

Lines 16-20 + 133 reference 5 fixed counters. Replace with a fetch from the new `/api/metrics/finish-reasons` endpoint:

```tsx
const { data: finishReasonData } = useFinishReasonTimeseries({ granularity, range })
```

- [ ] **Step 2: Add hook**

In `hooks/use-finish-reason-timeseries.ts` (new file):

```ts
import { useQuery } from "@tanstack/react-query"

export interface FinishReasonSeries {
  finish_reason: string
  points: [number, number][]
}

export function useFinishReasonTimeseries(args: {
  granularity: string
  range: { start: number; end: number }
}) {
  return useQuery<{ series: FinishReasonSeries[] }>({
    queryKey: ["finish-reasons", args],
    queryFn: async () => {
      const r = await fetch(
        `/api/metrics/finish-reasons?granularity=${args.granularity}&start=${args.range.start}&end=${args.range.end}`,
      )
      if (!r.ok) throw new Error(await r.text())
      return r.json()
    },
  })
}
```

- [ ] **Step 3: Render chart with tone-based colors**

In the chart component, color each series by `TONE_CLASS[finishTone(s.finish_reason)]` (translate tailwind class → hex via a small map for chart libraries that need explicit colors), and label each series with the raw name.

- [ ] **Step 4: bun typecheck + manual smoke**

Run: `bun run dev` — open `/traffic`, observe finish-reason chart shows raw values (`end_turn`, `pause_turn`, etc.).

### Task 6.7: Fixture cleanup

**Files:**
- Modify: `console/src/lib/turn-index.test.ts`

- [ ] **Step 1: Audit**

```bash
rg -n '"complete"|"length"|"error"' console/src/lib/turn-index.test.ts
```

Replace each occurrence acting as a `finish_reason` value with a raw provider value matching the fixture's `wire_api`:

- For `"complete"` (Anthropic fixture): `"end_turn"`
- For `"complete"` (OpenAI fixture): `"stop"`
- For `"length"` (OpenAI fixture): keep — `"length"` is OpenAI's raw value
- For `"error"`: depends on fixture intent — if the fixture is testing "what happens when terminal arrives that we'd visually flag red", change to a real provider value like `"refusal"` (Anthropic) or `"content_filter"` (OpenAI)

- [ ] **Step 2: Run frontend tests**

Run from `console/`: `bun test`
Expected: green.

- [ ] **Step 3: Commit frontend**

```bash
git add console/
git commit -m "refactor(console): show raw provider finish_reason values

- finish-tone.ts maps 19 known provider values across 5 visual tones
- FinishBadge / call-card / gantt switch to tone-based coloring
- finish-reason filter regrouped by wire_api (Anthropic / OpenAI Chat /
  OpenAI Responses) with provider-native vocabulary
- Traffic chart reads new /api/metrics/finish-reasons long-format
  endpoint; series labelled with raw values
- Drop 'Errors only' toggle (replaced by finish-reason filter +
  status-code filter)"
```

---

## Phase 7: Documentation + release

### Task 7.1: Schema doc

**Files:**
- Modify: `docs/design/07-schema.md`

- [ ] **Step 1: Open and locate `llm_calls.finish_reason` description**

Update to:

> `finish_reason VARCHAR` — Raw provider value, verbatim. Anthropic emits `end_turn`/`stop_sequence`/`max_tokens`/`tool_use`/`pause_turn`/`refusal`/`model_context_window_exceeded`; OpenAI Chat emits `stop`/`length`/`tool_calls`/`function_call`/`content_filter`; OpenAI Responses emits `completed`/`incomplete`/`failed`/`cancelled`. Interpret using the row's `wire_api`. The `WireApi::is_terminal` and `WireApi::is_tool_use` predicates encode wire-level semantics.

- [ ] **Step 2: Document `agent_turns.status` value space**

Two values: `complete`, `incomplete`. Note that the wire-level reason lives in `final_finish_reason: VARCHAR`.

- [ ] **Step 3: Add `llm_finish_metrics` table**

Add a section documenting columns and that it's long-format keyed by raw provider value.

- [ ] **Step 4: Commit**

```bash
git add docs/
git commit -m "docs(schema): finish_reason is now raw; add llm_finish_metrics"
```

### Task 7.2: Changelog

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add unreleased section**

```markdown
## [Unreleased]

### Breaking changes

- `llm_calls.finish_reason` value space changed from normalized labels (`complete`/`length`/`tool_use`/`error`/`cancelled`) to **raw provider values** (`end_turn`, `stop`, `STOP`, `pause_turn`, `refusal`, `model_context_window_exceeded`, etc.). Interpret using the row's `wire_api`. Historical rows written before this version still carry the old normalized values; queries that mix old and new data must handle both. No reverse migration is performed.
- `agent_turns.status` collapsed to `complete | incomplete`. Former `length`/`failed`/`cancelled` values are gone — the wire-level reason is preserved verbatim in `final_finish_reason`.
- `llm_metrics` table dropped 5 `finish_*_count` columns. The data lives in the new long-format `llm_finish_metrics(timestamp, source_id, granularity, wire_api, model, server_ip, finish_reason, count)` table. Existing databases drop the columns via idempotent `ALTER TABLE`.
- `GET /api/llm-calls?errors_only=true` removed. Use `status_code` and `finish_reason` filters instead.
- `GET /api/metrics/finish-reasons` added (long-format finish-reason timeseries).

### Fixed

- Anthropic `pause_turn` no longer counted as an error and no longer prematurely closes the agent turn.
- Anthropic `refusal` and `model_context_window_exceeded` now persist verbatim instead of being silently bucketed into `error`.
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): finish_reason raw-first refactor breaking changes"
```

---

## Self-Review

### Spec coverage

Checking each requirement from the conversation against tasks:

- ✅ `LlmCall.finish_reason: Option<String>` — Task 2.1
- ✅ Drop `FinishReason` enum — Task 2.1
- ✅ Wire APIs return raw values — Tasks 2.2, 2.3, 2.4
- ✅ `WireApi::is_terminal` + `is_tool_use` — Task 1.1
- ✅ `TurnStatus = Complete | Incomplete` — Task 3.1
- ✅ Tracker uses trait predicates — Task 3.2
- ✅ No `FinishClass` — confirmed absent throughout
- ✅ Long-table `llm_finish_metrics` — Tasks 4.1–4.3
- ✅ Drop `errors_only` end-to-end — Task 5.1
- ✅ API endpoint for finish-reason timeseries — Task 5.2
- ✅ Frontend `FinishBadge` colors via tone — Task 6.2
- ✅ Drop "Errors only" toggle — Task 6.3
- ✅ Wire-api-grouped filter — Task 6.4
- ✅ Call card / Gantt visual error via tone — Task 6.5
- ✅ Traffic chart reads long table — Task 6.6
- ✅ Schema doc + changelog — Tasks 7.1, 7.2

### Type consistency

- `LlmCall.finish_reason` → `Option<String>` (Task 2.1) — used as `as_deref()` everywhere downstream ✅
- `WireApi::is_terminal(&str) -> bool` (Task 1.1) — called with `as_deref().unwrap_or("")` or guarded by `if let Some(r) = ...` ✅
- `LlmFinishMetric` fields (Task 4.1) — referenced consistently in 4.2, 4.3, 5.2 ✅
- `FinishTone` type (Task 6.1) — referenced in 6.2, 6.5 ✅

### Known follow-ups (out of scope, not blocking)

- Historical `llm_metrics` data older than this refactor loses its 5 `finish_*_count` columns. If retention matters, run a one-shot SQL extract before deploy.
- Storage backends beyond DuckDB (PG / CH) need parallel changes — they're pluggable per `CLAUDE.md` but not present in the tree as of writing. Audit before merge.
- The `WireApi` trait predicate location may need `pub` visibility tweaks — confirm during Task 1.1.

---

## Execution order

Phases 1 → 2 → 3 → 4 → 5 → 6 → 7. Each phase ends with a green test run. Phases 1 and 6 (frontend tone module) could run in parallel by two agents but have no shared dependency, so the simple linear order is fine.
