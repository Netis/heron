# Pipeline Phase 2: Shard Turn Tracking + Metrics Aggregation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the Phase-1 single-`TurnTracker` + single-`MetricsAggregator` bottleneck by sharding both aggregators behind an identify-and-route stage, and moving storage writes into a dedicated sink stage.

**Architecture:** `LlmProcessor` (in `ts-llm`) takes ownership of profile identification via a relocated `ProfileRegistry` and tags each `LlmEvent::Complete` with an optional `CallIdentity`. After processing, each call becomes an `Arc<LlmCall>` (immutable, never mutated by downstream). `spawn_llm_stage` fans out three **independent** copies of the Arc per event: (a) every `LlmCall` goes directly to `calls_tx` (no turn-shard forwarding); (b) identified calls also go to one of `T` turn-shard tasks by `hash(session_id) % T`; (c) every `LlmEvent` goes to one of `M` metrics-shard tasks by `hash(provider, model, server_ip) % M`. Turn shards emit only `LlmTurn` rows (which now carry `call_ids: Vec<String>` — the turn→calls back-reference). `main.rs` becomes a pure composition root.

**Simplification vs. Phase 1:** `LlmCall` loses `session_id`, `turn_id`, `client_kind` (formerly mutated in-place by the tracker). `LlmTurn` gains `call_ids: Vec<String>`. The `llm_calls` schema drops the three columns; `llm_turns` gains `call_ids`. Project is pre-release — no migration.

**Tech Stack:** Rust, Tokio, tokio::sync::mpsc, cargo workspace (ts-llm, ts-turn, ts-metrics, ts-storage, ts-common, app/tokenscope), TDD with cargo test.

---

## File Structure (Before Tasks)

**New files:**
- `server/ts-llm/src/profile.rs` (moved from ts-turn)
- `server/ts-llm/src/profiles/mod.rs`, `claude_cli.rs`, `codex_cli.rs` (moved from ts-turn)
- `server/ts-turn/src/stage.rs` — `spawn_turn_stage`
- `server/ts-metrics/src/stage.rs` — `spawn_metrics_stage`
- `server/ts-storage/src/sink.rs` — `spawn_storage_sink_stage`

**Files with new content / signature changes:**
- `server/ts-llm/src/lib.rs` — new module declarations + re-exports
- `server/ts-llm/src/model.rs` — **slim `LlmCall`** (drop `session_id`, `turn_id`, `client_kind`); add `CallIdentity`, `IdentifiedCall`; convert `LlmEvent` to carry `Arc<LlmCall>` in struct variant
- `server/ts-llm/src/processor.rs` — run `ProfileRegistry::find` + `extract_ids`, build `CallIdentity`, wrap call in `Arc` before emit
- `server/ts-llm/src/stage.rs` — rewritten `spawn_llm_stage` signature + three-way Arc routing
- `server/ts-turn/src/lib.rs` — re-export profile types from ts-llm; remove local `profile` / `profiles` modules
- `server/ts-turn/src/model.rs` — **add `LlmTurn.call_ids: Vec<String>`**
- `server/ts-turn/src/tracker.rs` — new `ingest` signature `(&LlmCall, &CallIdentity)`, immutable; pushes `call.id.clone()` onto current turn's `call_ids`
- `server/ts-metrics/src/aggregator.rs` — match updated for Arc-based struct variant of `LlmEvent::Complete`
- `server/ts-metrics/src/lib.rs` — add `pub mod stage;`
- `server/ts-storage/src/lib.rs` — add `pub mod sink;`, re-exports
- `server/ts-storage/src/duckdb.rs` — **`llm_calls` drops 3 columns, `llm_turns` adds `call_ids`**; `WriteBuffer::push_call` takes `Arc<LlmCall>`
- `server/ts-common/src/config.rs` — add `TurnConfig.shard_count`, new `MetricsConfig { shard_count }`, new `StorageSinkConfig { channel_capacity, flush_interval_ms }`
- `server/app/tokenscope/src/main.rs` — rewritten composition root
- `server/app/dbview/src/calls.rs` — drop `session_id` / `turn_id` / `client_kind` print lines
- `server/ts-turn/tests/integration.rs` — wire through new stages

**Files deleted:**
- `server/ts-turn/src/profile.rs`
- `server/ts-turn/src/profiles/mod.rs`
- `server/ts-turn/src/profiles/claude_cli.rs`
- `server/ts-turn/src/profiles/codex_cli.rs`

---

## Task 1: Move ProfileRegistry from ts-turn to ts-llm

**Files:**
- Delete: `server/ts-turn/src/profile.rs`, `server/ts-turn/src/profiles/` (entire directory)
- Create: `server/ts-llm/src/profile.rs`, `server/ts-llm/src/profiles/{mod.rs,claude_cli.rs,codex_cli.rs}`
- Modify: `server/ts-llm/src/lib.rs`, `server/ts-turn/src/lib.rs`, `server/ts-turn/src/tracker.rs`
- Test: `server/ts-turn/src/tracker.rs` existing tests, `server/ts-llm/src/profile.rs` moved tests

This task **only moves files and fixes imports**. No signature changes. Existing behavior must be preserved.

- [ ] **Step 1: Copy `ts-turn/src/profile.rs` to `ts-llm/src/profile.rs` verbatim**

Run:
```bash
cp server/ts-turn/src/profile.rs server/ts-llm/src/profile.rs
```

- [ ] **Step 2: Copy the profiles directory to ts-llm**

Run:
```bash
cp -r server/ts-turn/src/profiles server/ts-llm/src/profiles
```

- [ ] **Step 3: Update `ts-llm/src/profiles/mod.rs` import of `ProfileRegistry`**

The file currently has `use crate::profile::ProfileRegistry;` — in the new location this path still works (`crate` resolves to `ts_llm`). Leave unchanged. Verify by opening the copy and confirming the line reads `use crate::profile::ProfileRegistry;`.

- [ ] **Step 4: Register the new modules in `ts-llm/src/lib.rs`**

Edit `server/ts-llm/src/lib.rs`. Replace contents with:

```rust
pub mod model;
pub mod processor;
pub mod profile;
pub mod profiles;
pub mod stage;

// Internal modules — not part of the public API.
pub(crate) mod anthropic;
pub(crate) mod detector;
pub(crate) mod openai;

pub use stage::spawn_llm_stage;
pub use profile::{ClientProfile, ExtractedIds, ProfileRegistry};
```

- [ ] **Step 5: Delete the originals in ts-turn**

Run:
```bash
rm server/ts-turn/src/profile.rs
rm -r server/ts-turn/src/profiles
```

- [ ] **Step 6: Replace ts-turn's local declarations with re-exports**

Edit `server/ts-turn/src/lib.rs`. Replace contents with:

```rust
//! Turn grouping: aggregates `LlmCall` into `LlmTurn` per client session.
//!
//! Header-explicit only — calls without a matching `ClientProfile` do not
//! participate in turn grouping.

pub mod model;
pub mod tracker;

pub use model::{LlmTurn, TurnKey, TurnStatus};
// ProfileRegistry moved to ts-llm in Phase 2; re-exported here to preserve
// callers that reference `ts_turn::profile::*` / `ts_turn::profiles::*`.
pub use ts_llm::profile::{self, ClientProfile, ExtractedIds, ProfileRegistry};
pub use ts_llm::profiles;
pub use tracker::{TurnEvent, TurnTracker};
```

- [ ] **Step 7: Fix `ts-turn/src/tracker.rs` import**

Edit `server/ts-turn/src/tracker.rs` line 5. Change:

```rust
use crate::profile::{ClientProfile, ProfileRegistry};
```

to:

```rust
use ts_llm::profile::{ClientProfile, ProfileRegistry};
```

- [ ] **Step 8: Fix `ts-turn/src/tracker.rs` test imports**

Still in `server/ts-turn/src/tracker.rs`, near the bottom `#[cfg(test)] mod tests` block there is `use crate::profiles;`. Change to:

```rust
use ts_llm::profiles;
```

- [ ] **Step 9: Verify ts-turn tests compile and pass**

Run: `cargo test -p ts-turn --lib`
Expected: all existing tests pass (25+ tests across profile.rs tests, tracker.rs tests, model tests).

- [ ] **Step 10: Verify ts-llm tests compile and pass**

Run: `cargo test -p ts-llm --lib`
Expected: all existing ts-llm tests pass, plus the profile tests that came along with the move.

- [ ] **Step 11: Verify workspace builds**

Run: `cargo build --workspace`
Expected: no errors.

- [ ] **Step 12: Commit**

```bash
git add server/ts-llm/src/profile.rs server/ts-llm/src/profiles/ server/ts-llm/src/lib.rs \
        server/ts-turn/src/lib.rs server/ts-turn/src/tracker.rs
git rm server/ts-turn/src/profile.rs server/ts-turn/src/profiles/*.rs server/ts-turn/src/profiles
git commit -m "refactor(phase2): move ProfileRegistry from ts-turn to ts-llm

Profile identification must happen in ts-llm so llm-proc can compute
the turn-shard routing key. ts-turn re-exports the types so external
callers (tests, main.rs) remain source-compatible."
```

---

## Task 2: Add `ProfileRegistry::find_by_name` helper

**Files:**
- Modify: `server/ts-llm/src/profile.rs` (add method + test)

`TurnTracker::ingest` no longer runs `find()` (which iterates `matches()` on every profile). Instead, llm-proc does `find()` once and passes the matched profile name; the tracker looks up the profile by name to call the per-profile semantics methods (`is_user_turn_start`, `extract_user_input`, etc.).

- [ ] **Step 1: Write failing test for `find_by_name`**

Add to `server/ts-llm/src/profile.rs` inside the existing `#[cfg(test)] mod tests` block (keep existing tests):

```rust
    #[test]
    fn find_by_name_returns_matching_profile() {
        let reg = ProfileRegistry::new()
            .with(Box::new(FakeProfile { ua_prefix: "alpha/", name: "alpha" }))
            .with(Box::new(FakeProfile { ua_prefix: "beta/", name: "beta" }));
        assert_eq!(reg.find_by_name("alpha").map(|p| p.name()), Some("alpha"));
        assert_eq!(reg.find_by_name("beta").map(|p| p.name()), Some("beta"));
        assert!(reg.find_by_name("gamma").is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ts-llm --lib profile::tests::find_by_name`
Expected: FAIL with "no method named `find_by_name`".

- [ ] **Step 3: Implement `find_by_name`**

Edit `server/ts-llm/src/profile.rs`. Inside `impl ProfileRegistry`, add after the existing `find` method:

```rust
    pub fn find_by_name(&self, name: &str) -> Option<&dyn ClientProfile> {
        self.profiles.iter().map(|p| p.as_ref()).find(|p| p.name() == name)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ts-llm --lib profile::tests`
Expected: all pass including `find_by_name_returns_matching_profile`.

- [ ] **Step 5: Commit**

```bash
git add server/ts-llm/src/profile.rs
git commit -m "feat(ts-llm): add ProfileRegistry::find_by_name

Needed by TurnTracker to look up a profile by its stored name without
re-scanning matches()."
```

---

## Task 3: Slim `LlmCall`, extend `LlmTurn.call_ids`, update DuckDB schema

**Files:**
- Modify: `server/ts-llm/src/model.rs` — drop 3 fields from `LlmCall`
- Modify: `server/ts-turn/src/model.rs` — add `call_ids: Vec<String>` to `LlmTurn`
- Modify: `server/ts-storage/src/duckdb.rs` — drop 3 columns from `llm_calls`, add `call_ids` column to `llm_turns`, update insert and test fixtures
- Modify: `server/ts-llm/src/processor.rs` — stop setting the 3 removed fields (they're already `None` today, but the literal goes away)
- Modify: `server/ts-turn/src/tracker.rs` — stop mutating the 3 removed fields (Phase 1 code that goes away — will be done properly in Task 6 but the struct-init call sites have to compile now)
- Modify: `server/ts-metrics/src/aggregator.rs` — test helpers that literal-init `LlmCall`
- Modify: `server/app/dbview/src/calls.rs` — drop 3 print lines
- Modify: `server/app/tokenscope/src/main.rs` — drop any usage of the removed fields
- Any other site that literal-constructs `LlmCall` (tests, fixtures)

This task is purely a structural rename/removal. Does NOT introduce routing behavior — it only prepares `LlmCall` to be the immutable post-llm-proc snapshot.

- [ ] **Step 1: Write failing test asserting `LlmTurn.call_ids` exists**

Add to `server/ts-turn/src/model.rs` `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn llm_turn_has_call_ids() {
        let turn = LlmTurn {
            turn_id: "t".into(), session_id: "s".into(), tenant_id: None,
            provider: "anthropic".into(), client_kind: "claude-cli".into(),
            start_time_us: 0, end_time_us: 0, duration_ms: 0,
            call_count: 2, models_used: vec![], subagents_used: vec![],
            total_input_tokens: 0, total_output_tokens: 0,
            total_cached_input_tokens: 0, total_cost_usd: None,
            status: TurnStatus::Complete, final_finish_reason: None,
            user_input: None, final_answer_preview: None, final_call_id: None,
            call_ids: vec!["call-1".into(), "call-2".into()],
            metadata: serde_json::json!({}),
        };
        assert_eq!(turn.call_ids.len(), 2);
        assert_eq!(turn.call_ids[0], "call-1");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ts-turn --lib model::tests::llm_turn_has_call_ids`
Expected: FAIL with "no field `call_ids`".

- [ ] **Step 3: Add `call_ids` to `LlmTurn`**

Edit `server/ts-turn/src/model.rs`. In the `LlmTurn` struct (around line 43-76), add after `final_call_id`:

```rust
    /// Ordered list of `LlmCall.id` values that belong to this turn.
    /// Populated by `TurnTracker::ingest` in ingest order. Replaces the
    /// Phase-1 `LlmCall.turn_id` back-reference.
    pub call_ids: Vec<String>,
```

Also update the existing `llm_turn_display_includes_key_fields` test literal: add `call_ids: vec![],` to the struct initializer.

- [ ] **Step 4: Remove `session_id`, `turn_id`, `client_kind` from `LlmCall`**

Edit `server/ts-llm/src/model.rs`. Delete the three fields (they live near the end of the struct). The struct should no longer carry any turn-routing information — those moved to `CallIdentity` (Task 4).

Also delete any `session_id: None, turn_id: None, client_kind: None` lines in this file's tests that initialize `LlmCall`.

- [ ] **Step 5: Update `TurnTracker::ingest` Phase-1 mutation site to compile**

Edit `server/ts-turn/src/tracker.rs`. Find the block that sets `call.session_id = ...`, `call.turn_id = ...`, `call.client_kind = ...` (currently inside `ingest`). Delete those three assignments. Task 6 replaces the whole `ingest` signature; this step just removes the now-invalid field accesses so the workspace compiles.

- [ ] **Step 6: Update `LlmProcessor` construction site**

Edit `server/ts-llm/src/processor.rs`. Find the `LlmCall { ... }` literal in `on_response` (around line 150-200). Delete the `session_id: None, turn_id: None, client_kind: None,` lines.

- [ ] **Step 7: Update all other `LlmCall` literal constructions**

Search for `session_id: ` within `LlmCall { ... }` literals. Known sites (delete the 3 fields from each):
- `server/ts-metrics/src/aggregator.rs` `make_complete` (~line 272)
- `server/ts-storage/src/duckdb.rs` test fixtures (~line 993, 1059, 1657 and similar)
- Any test fixture in `ts-llm/src/processor.rs` under `#[cfg(test)]`
- `server/ts-llm/src/stage.rs` tests if they literal-construct `LlmCall`

Run `grep -rn "session_id: " server/` scoped to `LlmCall` contexts; if in doubt delete then let `cargo check` guide you.

- [ ] **Step 8: Update DuckDB `llm_calls` schema**

Edit `server/ts-storage/src/duckdb.rs`. In the `CREATE TABLE llm_calls` DDL (around line 60), remove the three columns:

```
    session_id        VARCHAR,
    turn_id           VARCHAR,
    client_kind       VARCHAR,
```

In the `INSERT INTO llm_calls` statement + param binding (around line 340+), remove `call.session_id.clone()`, `call.turn_id.clone()`, `call.client_kind.clone()` and the corresponding column names + `?` placeholders. Adjust column count.

- [ ] **Step 9: Update DuckDB `llm_turns` schema**

In the `CREATE TABLE llm_turns` DDL (around line 113), add a `call_ids` column. Pattern to mirror `models_used` / `subagents_used`:

```
    call_ids               JSON NOT NULL,
```

(DuckDB supports `JSON` type; store as `serde_json::to_string(&turn.call_ids)`.)

In the `INSERT INTO llm_turns` statement + param binding (around line 435), add `serde_json::to_string(&t.call_ids)?` binding and corresponding column/placeholder.

- [ ] **Step 10: Update DuckDB `llm_calls`/`llm_turns` test assertions**

Same file, existing test at line 1066 `"SELECT session_id, turn_id, client_kind FROM llm_calls"` — delete the entire test (it tested a field that no longer exists). Or repurpose it: keep as `SELECT id FROM llm_calls` sanity check if symmetric coverage is wanted.

Existing `LlmTurn` fixture literals must add `call_ids: vec![],`.

- [ ] **Step 11: Update `dbview/calls.rs`**

Edit `server/app/dbview/src/calls.rs`. Remove:
- The three fields from the `CallDetail` struct (lines ~126-128)
- The three column names from the `SELECT` (line 141)
- The three row-gets (lines 172-174)
- The three print lines (lines 186-188)

- [ ] **Step 12: Run full workspace build**

Run: `cargo check --workspace`
Expected: clean. If lingering references remain, fix them one by one.

- [ ] **Step 13: Run full workspace tests**

Run: `cargo test --workspace`
Expected: all tests green. The new `llm_turn_has_call_ids` test passes; no Phase-1 test regresses (except the removed `session_id` selector test from Step 10).

- [ ] **Step 14: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor(phase2): drop LlmCall turn/session/client_kind; add LlmTurn.call_ids

Reverse the turn↔calls back-reference so LlmCall becomes immutable
post-llm-proc. LlmTurn.call_ids (JSON column) replaces llm_calls.turn_id
as the relation between tables. dbview and test fixtures follow.

No migration path provided (pre-release project). Downstream Phase-2 tasks
will leverage this immutability to fan out Arc<LlmCall> safely.
EOF
)"
```

---

## Task 3b: Add `CallIdentity` + `IdentifiedCall` types, convert `LlmEvent::Complete` to Arc-based struct variant

**Files:**
- Modify: `server/ts-llm/src/model.rs`
- Modify: `server/ts-llm/src/processor.rs` (emission point + existing tests)
- Modify: `server/ts-llm/src/stage.rs` (existing test patterns)
- Modify: `server/ts-metrics/src/aggregator.rs` (match pattern + test helper)
- Modify: `server/ts-turn/tests/integration.rs` (match pattern)
- Modify: `server/app/tokenscope/src/main.rs` (match pattern)

This is a breaking variant change. Touch every pattern-match site in one task to keep the workspace green.

- [ ] **Step 1: Write failing test for the new types**

Add to `server/ts-llm/src/model.rs` inside `#[cfg(test)] mod extension_tests` (keep existing tests):

```rust
    use std::sync::Arc;

    #[test]
    fn call_identity_round_trips() {
        let id = CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".to_string(),
            session_id: "sess-1".to_string(),
            turn_id_hint: None,
        };
        assert_eq!(id.profile_name, "claude-cli");
        assert_eq!(id.session_id, "sess-1");
        assert!(id.turn_id_hint.is_none());
    }

    #[test]
    fn identified_call_carries_arc_and_identity() {
        let call = LlmCall {
            id: "c".into(),
            provider: ProviderFormat::Anthropic,
            model: "claude".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 0, response_time: None, complete_time: None,
            request_path: "/".into(), is_stream: false, request_body: None,
            status_code: None, finish_reason: None, response_body: None,
            input_tokens: None, output_tokens: None, total_tokens: None,
            ttfb_ms: None, e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(), client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(), server_port: 0,
            response_id: None,
            request_headers: vec![], response_headers: vec![],
        };
        let arc = Arc::new(call);
        let id = CallIdentity {
            profile_name: "x", client_kind: "x".into(),
            session_id: "s".into(), turn_id_hint: None,
        };
        let ic = IdentifiedCall { call: Arc::clone(&arc), identity: id };
        assert_eq!(ic.call.id, "c");
        assert_eq!(ic.identity.session_id, "s");
        // The Arc is shared — still alive here.
        assert_eq!(Arc::strong_count(&arc), 2);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ts-llm --lib model::extension_tests::call_identity_round_trips`
Expected: FAIL with "cannot find type `CallIdentity`".

- [ ] **Step 3: Define `CallIdentity` + `IdentifiedCall` + Arc-based struct-variant `LlmEvent`**

Edit `server/ts-llm/src/model.rs`. Add the `use std::sync::Arc;` at the top if not present. Replace the existing `LlmEvent` enum definition (lines 108-115) with:

```rust
/// Stable identity of an LLM call once a `ClientProfile` has matched.
/// The turn shard uses `profile_name` to look up the profile for
/// per-profile semantics (is_user_turn_start, extract_user_input, ...).
#[derive(Debug, Clone)]
pub struct CallIdentity {
    pub profile_name: &'static str,
    pub client_kind: String,
    pub session_id: String,
    /// Explicit turn_id from request body when the profile provides one (e.g. Codex).
    /// `None` when the turn shard will generate the turn_id (Anthropic path).
    pub turn_id_hint: Option<String>,
}

/// An LlmCall packaged with its extracted identity. The `call` is an `Arc`
/// because the same `LlmCall` is fanned out to the storage sink and
/// (for identified calls) the turn shard in parallel — all consumers
/// read-only, no mutex needed.
#[derive(Debug, Clone)]
pub struct IdentifiedCall {
    pub call: Arc<LlmCall>,
    pub identity: CallIdentity,
}

/// Event emitted by the LLM processor for downstream consumption.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    /// A new LLM API request has been detected (for concurrency tracking).
    Start(LlmCallStart),
    /// An LLM API call has been fully completed (request + response paired).
    /// `identity` is `Some` iff a `ClientProfile` matched and extracted session info.
    Complete { call: Arc<LlmCall>, identity: Option<CallIdentity> },
}
```

- [ ] **Step 4: Update `LlmProcessor::process` emission site**

Edit `server/ts-llm/src/processor.rs`. Add `use std::sync::Arc;` at the top. At line ~55, change:

```rust
                match self.on_response(resp) {
                    Some(call) => vec![LlmEvent::Complete(call)],
                    None => Vec::new(),
                }
```

to (leave identity extraction for Task 4 — for now just `None`):

```rust
                match self.on_response(resp) {
                    Some(call) => vec![LlmEvent::Complete { call: Arc::new(call), identity: None }],
                    None => Vec::new(),
                }
```

- [ ] **Step 5: Update pattern matches in `ts-llm/src/processor.rs` tests**

In the same file, find every `LlmEvent::Complete(call)` pattern inside `#[cfg(test)] mod tests` and change to `LlmEvent::Complete { call, .. }`. Exact locations:
- Line ~346: `LlmEvent::Complete(call) => {` → `LlmEvent::Complete { call, .. } => {`
- Line ~406: same
- Line ~461: same
- Line ~556: same

(Leave the destructured `call` name unchanged; only the wrapper pattern changes.)

- [ ] **Step 6: Update pattern matches in `ts-llm/src/stage.rs` tests**

Edit `server/ts-llm/src/stage.rs`. Find the two test assertions that match `LlmEvent::Complete(...)`:
- Line ~116: `LlmEvent::Complete(_) => panic!(...)` → `LlmEvent::Complete { .. } => panic!(...)`
- Line ~119: `LlmEvent::Complete(call) => { ... }` → `LlmEvent::Complete { call, .. } => { ... }`
- Line ~126: `LlmEvent::Start(_) => panic!(...)` — unchanged
- In `four_receivers_parallel_four_flows` test (~line 160): `LlmEvent::Complete(_) => completes += 1` → `LlmEvent::Complete { .. } => completes += 1`

- [ ] **Step 7: Update `ts-metrics/src/aggregator.rs` production match**

Edit `server/ts-metrics/src/aggregator.rs`. Line ~82 in `MetricsAggregator::process`:

```rust
            LlmEvent::Complete(call) => {
                self.on_call_complete(call);
                self.latest_ts = self.latest_ts.max(call.request_time);
                self.check_windows()
            }
```

Change to (note: `call` is now `&Arc<LlmCall>` — deref coerces to `&LlmCall` for method calls):

```rust
            LlmEvent::Complete { call, .. } => {
                self.on_call_complete(call.as_ref());
                self.latest_ts = self.latest_ts.max(call.request_time);
                self.check_windows()
            }
```

If `on_call_complete` signature is `fn on_call_complete(&self, call: &LlmCall)`, the `.as_ref()` is explicit for clarity. Alternatively rely on auto-deref: `self.on_call_complete(call)` works if the param is `&LlmCall` (since `&Arc<T>` derefs to `&T`).

- [ ] **Step 8: Update `ts-metrics/src/aggregator.rs` test `make_complete` helper**

Same file, function `make_complete` at line ~272 currently returns `LlmEvent::Complete(LlmCall { ... })`. Change to Arc-wrapped struct variant (the 3 removed fields are already gone from Task 3):

```rust
    fn make_complete(request_time: i64, complete_time: i64, model: &str) -> LlmEvent {
        LlmEvent::Complete {
            call: Arc::new(LlmCall {
                id: "test".to_string(),
                provider: ProviderFormat::OpenAI,
                model: model.to_string(),
                api_type: ApiType::Chat,
                tenant_id: None,
                request_time,
                response_time: Some(request_time + 100_000),
                complete_time: Some(complete_time),
                request_path: "/v1/chat/completions".to_string(),
                is_stream: true,
                request_body: None,
                status_code: Some(200),
                finish_reason: Some(FinishReason::Complete),
                response_body: None,
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                ttfb_ms: Some(100.0),
                e2e_latency_ms: Some(500.0),
                client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
                client_port: 12345,
                server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
                server_port: 443,
                response_id: None,
                request_headers: vec![],
                response_headers: vec![],
            }),
            identity: None,
        }
    }
```

Add `use std::sync::Arc;` at the top of the test module or file.

- [ ] **Step 9: Update `ts-turn/tests/integration.rs` match**

Edit `server/ts-turn/tests/integration.rs` line 74:

```rust
        if let LlmEvent::Complete(mut call) = llm_event {
```

Change to (call is now `Arc<LlmCall>` — no longer mutable):

```rust
        if let LlmEvent::Complete { call, .. } = llm_event {
```

Note: downstream `tracker.ingest(&mut call)` will stop compiling — fine, Task 6 updates the tracker signature to take `&LlmCall` and fixes this call site.

- [ ] **Step 10: Update `app/tokenscope/src/main.rs` match**

Edit `server/app/tokenscope/src/main.rs` line ~350:

```rust
                                LlmEvent::Complete(call) => {
```

Change to:

```rust
                                LlmEvent::Complete { call, .. } => {
```

`call` here is now `Arc<LlmCall>`. Phase-1-era code that mutated or moved `call` (e.g. `tracker.ingest(&mut call)`, or push-to-write-buffer) will break. Leave it broken — Task 12 rewrites `main.rs` entirely. Add a temporary `let _ = call;` or `unreachable!("rewritten in Task 12")` as needed to make the file compile for the intermediate commit. **Recommended shortcut**: replace the entire `match` arm body with `let _ = call; /* rewired in Task 12 */` — this is the smallest edit that keeps the workspace green.

- [ ] **Step 11: Verify workspace builds and all tests pass**

Run: `cargo test --workspace`
Expected: all prior tests pass plus the two new `CallIdentity` tests.

- [ ] **Step 12: Commit**

```bash
git add server/ts-llm/src/model.rs server/ts-llm/src/processor.rs server/ts-llm/src/stage.rs \
        server/ts-metrics/src/aggregator.rs server/ts-turn/tests/integration.rs \
        server/app/tokenscope/src/main.rs
git commit -m "feat(ts-llm): add CallIdentity, IdentifiedCall; LlmEvent::Complete is struct variant

Prepares for Phase 2 fan-out routing where identified calls are wrapped
into IdentifiedCall for turn shards. Identity is still always None at
this point; llm-proc will fill it in the next commit."
```

---

## Task 4: `LlmProcessor` computes `CallIdentity`

**Files:**
- Modify: `server/ts-llm/src/processor.rs` (new field, new param, identity builder, tests)

The processor now owns `Arc<ProfileRegistry>` and runs `find` + `extract_ids` after each Complete to build a `CallIdentity`. **The `LlmCall` is not mutated** — identity info lives on `CallIdentity` only (the three fields that used to be annotated on `LlmCall` were removed in Task 3). For the Codex path, `turn_id_hint` is populated from `extract_ids`; for the Anthropic path, `turn_id_hint` is `None` (the turn shard will generate a `turn_id` for the `LlmTurn` row).

- [ ] **Step 1: Write failing test — identified Complete carries identity + annotated call**

Add to `server/ts-llm/src/processor.rs` inside `#[cfg(test)] mod tests`, after existing tests:

```rust
    #[test]
    fn complete_for_claude_cli_attaches_identity() {
        use crate::profiles::build_default_registry;
        use std::sync::Arc;

        let registry = Arc::new(build_default_registry());
        let mut proc = LlmProcessor::new(registry);

        let (client, server) = addr();
        let body = serde_json::json!({
            "model": "claude-sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        let req = HttpRequestData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            version: 1,
            headers: vec![
                ("user-agent".to_string(), "claude-cli/2.1.98".to_string()),
                ("x-claude-code-session-id".to_string(), "sess-xyz".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: 1_000_000,
        };
        proc.process(ProtocolEvent::HttpRequest(req));

        let resp_body = serde_json::json!({
            "id": "msg_01",
            "model": "claude-sonnet",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let events = proc.process(ProtocolEvent::HttpResponse(http_response(&resp_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call, identity } => {
                let id = identity.as_ref().expect("claude-cli should match");
                assert_eq!(id.profile_name, "claude-cli");
                assert_eq!(id.client_kind, "claude-cli");
                assert_eq!(id.session_id, "sess-xyz");
                assert_eq!(id.turn_id_hint, None, "anthropic path has no explicit turn_id");
                // Call body is immutable; we only assert it's the shared Arc.
                assert_eq!(call.id.len() > 0, true);
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn complete_without_profile_match_has_no_identity() {
        use crate::profiles::build_default_registry;
        use std::sync::Arc;

        let registry = Arc::new(build_default_registry());
        let mut proc = LlmProcessor::new(registry);

        // Plain openai request with no claude-cli / codex-cli signature.
        let req_body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        });
        proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));

        let resp_body = serde_json::json!({
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        });
        let events = proc.process(ProtocolEvent::HttpResponse(http_response(&resp_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call: _, identity } => {
                assert!(identity.is_none(), "no profile should match");
            }
            _ => panic!("expected Complete"),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ts-llm --lib processor::tests::complete_for_claude_cli_attaches_identity processor::tests::complete_without_profile_match_has_no_identity`
Expected: FAIL with "expected 1 argument, found 0" on `LlmProcessor::new` — the signature will change in Step 3.

- [ ] **Step 3: Add `registry` field + parameter to `LlmProcessor`**

Edit `server/ts-llm/src/processor.rs`. At the top, add import:

```rust
use std::sync::Arc;

use crate::profile::ProfileRegistry;
```

Change the struct and constructor:

```rust
/// Processes ProtocolEvents and extracts LlmCall records.
pub struct LlmProcessor {
    pending: HashMap<FlowKey, PendingCall>,
    call_count: u64,
    registry: Arc<ProfileRegistry>,
}

impl LlmProcessor {
    pub fn new(registry: Arc<ProfileRegistry>) -> Self {
        Self {
            pending: HashMap::new(),
            call_count: 0,
            registry,
        }
    }
```

- [ ] **Step 4: Build identity in the `Complete` emission path**

Edit `server/ts-llm/src/processor.rs`. The `process` function's `HttpResponse` arm currently returns `vec![LlmEvent::Complete { call: Arc::new(call), identity: None }]`. Replace that entire arm:

```rust
            ProtocolEvent::HttpResponse(resp) => {
                match self.on_response(resp) {
                    Some(call) => {
                        let identity = self.build_identity(&call);
                        vec![LlmEvent::Complete { call: Arc::new(call), identity }]
                    }
                    None => Vec::new(),
                }
            }
```

- [ ] **Step 5: Implement `build_identity`**

Edit `server/ts-llm/src/processor.rs`. Add this method to `impl LlmProcessor` (anywhere after `new`):

```rust
    /// Identify the client profile for this call.
    /// Returns `Some(CallIdentity)` iff a profile matched and produced ids.
    /// Does NOT mutate the call — identity lives on the returned struct only.
    fn build_identity(&self, call: &crate::model::LlmCall) -> Option<crate::model::CallIdentity> {
        let profile = self.registry.find(call)?;
        let ids = profile.extract_ids(call)?;
        let name = profile.name();
        Some(crate::model::CallIdentity {
            profile_name: name,
            client_kind: name.to_string(),
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        })
    }
```

Note on the `ExtractedIds` field name: the profile trait's `extract_ids` returns a struct with `session_id: String` and `turn_id: Option<String>`. We rename only the Identity struct field (`turn_id_hint`) — the profile-side contract stays `turn_id` for backward compat with existing profile implementations.

- [ ] **Step 6: Fix existing ts-llm processor tests that construct `LlmProcessor::new()`**

Same file. Every existing test in `mod tests` that starts with `let mut proc = LlmProcessor::new();` must now pass a registry. The simplest is an empty one so old tests keep seeing `identity: None`. At the top of the `mod tests` block (after the `fn sse_event` helper), add:

```rust
    fn empty_registry() -> std::sync::Arc<crate::profile::ProfileRegistry> {
        std::sync::Arc::new(crate::profile::ProfileRegistry::new())
    }
```

Then find every `LlmProcessor::new()` inside tests and replace with `LlmProcessor::new(empty_registry())`. Exact locations (one per):
- `test_openai_chat_non_streaming` (~line 324)
- `test_openai_chat_streaming` (~line 366)
- `test_anthropic_streaming` (~line 420)
- `test_response_without_request_ignored` (~line 477)
- `test_cleanup_stale_pending` (~line 487)
- `test_non_llm_request_ignored` (~line 506)
- `test_headers_and_response_id_passed_through` (~line 527)

- [ ] **Step 7: Run tests to verify everything passes**

Run: `cargo test -p ts-llm --lib`
Expected: all prior tests pass plus the two new identity tests.

- [ ] **Step 8: Commit**

```bash
git add server/ts-llm/src/processor.rs
git commit -m "feat(ts-llm): LlmProcessor attaches CallIdentity via ProfileRegistry

LlmProcessor now owns Arc<ProfileRegistry>; after assembling a Complete,
it calls registry.find + profile.extract_ids and packages the result as
CallIdentity alongside the Arc<LlmCall>. LlmCall itself is not mutated —
identity is a sidecar, enabling downstream fan-out via Arc clones."
```

---

## Task 5: Update `spawn_llm_stage` signature (still single output)

**Files:**
- Modify: `server/ts-llm/src/stage.rs` (new params for registry + output_tx; tests updated)
- Modify: `server/ts-turn/tests/integration.rs` (pass registry)
- Modify: `server/app/tokenscope/src/main.rs` (pass registry)

Step toward the full fan-out: the stage now takes a `registry: Arc<ProfileRegistry>` and plumbs it into each spawned `LlmProcessor`. The output channel is still a single `mpsc::Sender<LlmEvent>` — Phase 2 full routing comes in Task 10 once the per-shard stages exist.

- [ ] **Step 1: Write failing test — stage-spawned processors carry identity**

Edit `server/ts-llm/src/stage.rs`. Add test inside `#[cfg(test)] mod tests` (keep existing):

```rust
    #[tokio::test]
    async fn stage_attaches_identity_from_registry() {
        use crate::profiles::build_default_registry;
        use std::sync::Arc;

        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);
        let (out_tx, mut out_rx) = mpsc::channel::<LlmEvent>(16);

        spawn_llm_stage(vec![event_rx], out_tx, Arc::new(build_default_registry()));

        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let fk = FlowKey::new(ip, 5000, ip, 8080);
        let body = serde_json::json!({
            "model": "claude-sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        let req = HttpRequestData {
            flow_key: fk.clone(),
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            version: 1,
            headers: vec![
                ("user-agent".to_string(), "claude-cli/2.1.98".to_string()),
                ("x-claude-code-session-id".to_string(), "S".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: 1_000_000,
        };
        let resp_body = serde_json::json!({
            "id": "msg_01",
            "model": "claude-sonnet",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let resp = HttpResponseData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(resp_body.to_string()),
            first_byte_timestamp_us: 1_100_000,
            complete_timestamp_us: 1_200_000,
        };
        event_tx.send(ProtocolEvent::HttpRequest(req)).await.unwrap();
        event_tx.send(ProtocolEvent::HttpResponse(resp)).await.unwrap();
        drop(event_tx);

        let _ = out_rx.recv().await.expect("Start");
        match out_rx.recv().await.expect("Complete") {
            LlmEvent::Complete { call: _, identity } => {
                let id = identity.as_ref().expect("should match");
                assert_eq!(id.profile_name, "claude-cli");
                assert_eq!(id.session_id, "S");
            }
            _ => panic!("expected Complete"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ts-llm --lib stage::tests::stage_attaches_identity_from_registry`
Expected: FAIL with wrong arg count for `spawn_llm_stage`.

- [ ] **Step 3: Update `spawn_llm_stage` signature**

Edit `server/ts-llm/src/stage.rs`. Replace the function with:

```rust
use std::sync::Arc;

use tokio::sync::mpsc;

use ts_protocol::model::ProtocolEvent;

use crate::model::LlmEvent;
use crate::processor::LlmProcessor;
use crate::profile::ProfileRegistry;

/// Spawn N parallel LLM-extraction tasks, one per input receiver. Each task
/// owns its own `LlmProcessor` (sharing the `ProfileRegistry` via `Arc`) and
/// forwards emitted `LlmEvent`s into a clone of `output_tx`. Tasks exit
/// cleanly when their input channel closes.
pub fn spawn_llm_stage(
    event_rxs: Vec<mpsc::Receiver<ProtocolEvent>>,
    output_tx: mpsc::Sender<LlmEvent>,
    registry: Arc<ProfileRegistry>,
) {
    for mut rx in event_rxs {
        let tx = output_tx.clone();
        let reg = registry.clone();
        tokio::spawn(async move {
            let mut processor = LlmProcessor::new(reg);
            while let Some(event) = rx.recv().await {
                for llm_event in processor.process(event) {
                    if tx.send(llm_event).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
}
```

- [ ] **Step 4: Fix the existing three stage tests**

Same file. Each existing test creates `spawn_llm_stage(event_rxs, out_tx);` — must now pass a registry. Update:
- `single_receiver_emits_start_and_complete`: `spawn_llm_stage(vec![event_rx], out_tx, Arc::new(ProfileRegistry::new()));`
- `four_receivers_parallel_four_flows`: `spawn_llm_stage(event_rxs, out_tx, Arc::new(ProfileRegistry::new()));`
- `tasks_exit_when_input_channels_drop`: same.

Add `use std::sync::Arc; use crate::profile::ProfileRegistry;` at the top of the `mod tests` block.

- [ ] **Step 5: Update `ts-turn/tests/integration.rs` to pass the registry**

Edit `server/ts-turn/tests/integration.rs`. Line 54:

```rust
    ts_llm::spawn_llm_stage(vec![event_rx], llm_tx);
```

Change to:

```rust
    use std::sync::Arc;
    ts_llm::spawn_llm_stage(vec![event_rx], llm_tx, Arc::new(ts_llm::profiles::build_default_registry()));
```

(Add the `use std::sync::Arc;` import at the top of the file if not already present.)

- [ ] **Step 6: Update `app/tokenscope/src/main.rs` to pass the registry**

Edit `server/app/tokenscope/src/main.rs` line ~292:

```rust
        ts_llm::spawn_llm_stage(event_rxs, llm_tx);
```

Change to:

```rust
        use std::sync::Arc;
        let registry = Arc::new(ts_llm::profiles::build_default_registry());
        ts_llm::spawn_llm_stage(event_rxs, llm_tx, registry.clone());
```

(The existing `let turn_registry = build_default_registry();` at line ~335 builds a separate registry for the tracker — leave it untouched in this task; Task 11 rewires main.rs wholesale.)

- [ ] **Step 7: Run tests + build**

Run: `cargo test --workspace` then `cargo build --workspace`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add server/ts-llm/src/stage.rs server/ts-turn/tests/integration.rs server/app/tokenscope/src/main.rs
git commit -m "feat(ts-llm): spawn_llm_stage accepts Arc<ProfileRegistry>

Every llm-proc task now shares the same registry. Output is still a
single LlmEvent channel; full fan-out routing comes with spawn_turn_stage
and spawn_metrics_stage."
```

---

## Task 6: `TurnTracker::ingest` new signature — read-only `&LlmCall`, consume `CallIdentity`, populate `call_ids`

**Files:**
- Modify: `server/ts-turn/src/tracker.rs` (new signature, immutable body, `ActiveTurn.call_ids`, tests updated)

The tracker no longer runs `registry.find()` or `profile.extract_ids()` — `identity.profile_name` is used to look up the profile directly. **The call is now `&LlmCall` (immutable)**; the tracker never writes back to it. Per-turn `call_ids: Vec<String>` is maintained on `ActiveTurn` and flows into the finalized `LlmTurn`.

- [ ] **Step 1: Write failing test — ingest with pre-identified call**

Edit `server/ts-turn/src/tracker.rs`. Add to `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn ingest_with_identity_skips_registry_find() {
        use ts_llm::model::CallIdentity;

        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(reg, TrackerConfig::default());

        let call = anthropic_call("S", 1_000_000, "text", FinishReason::Complete);
        let identity = CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".into(),
            session_id: "S".into(),
            turn_id_hint: None,
        };

        let events = t.ingest(&call, &identity);
        assert!(events.iter().any(|e| matches!(e, TurnEvent::Started { .. })));
        assert!(events.iter().any(|e| matches!(e, TurnEvent::CallAdded { .. })));
    }

    #[test]
    fn ingest_populates_call_ids_into_finalized_turn() {
        use ts_llm::model::CallIdentity;

        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(reg, TrackerConfig::default());

        let c1 = codex_call("s1", "t1", "hello");
        let c2 = codex_call("s1", "t1", "world");
        let id1 = CallIdentity {
            profile_name: "codex-cli", client_kind: "codex-cli".into(),
            session_id: "s1".into(), turn_id_hint: Some("t1".into()),
        };
        let id2 = id1.clone();
        t.ingest(&c1, &id1);
        t.ingest(&c2, &id2);
        let finalized: Vec<_> = t.flush_all().into_iter()
            .filter_map(|e| match e { TurnEvent::Completed(t) => Some(t), _ => None })
            .collect();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].call_ids, vec![c1.id.clone(), c2.id.clone()]);
    }

    #[test]
    fn ingest_with_identity_honors_explicit_turn_id() {
        use ts_llm::model::CallIdentity;

        let reg = profiles::build_default_registry();
        let mut t = TurnTracker::new(reg, TrackerConfig::default());

        let call = codex_call("s1", "t1", "message");
        let identity = CallIdentity {
            profile_name: "codex-cli",
            client_kind: "codex-cli".into(),
            session_id: "s1".into(),
            turn_id_hint: Some("t1".into()),
        };
        let events = t.ingest(&call, &identity);
        assert!(events.iter().any(|e| matches!(e, TurnEvent::Started { .. })));
        assert_eq!(t.active_count(), 1);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ts-turn --lib tracker::tests::ingest_with_identity_skips_registry_find tracker::tests::ingest_with_identity_honors_explicit_turn_id`
Expected: FAIL — `ingest` expects 1 argument.

- [ ] **Step 3: Add `call_ids: Vec<String>` to `ActiveTurn` and finalize**

Edit `server/ts-turn/src/tracker.rs`. Find the `ActiveTurn` struct (inside the tracker module) and add `pub call_ids: Vec<String>`. Update the `ActiveTurn::finalize` method to populate `LlmTurn.call_ids: self.call_ids.clone()`. Update `ActiveTurn::merge` to push `call.id.clone()` onto `self.call_ids`.

- [ ] **Step 4: Rewrite `TurnTracker::ingest` with new signature**

Same file. Replace the existing `pub fn ingest` (~line 146) with the immutable version below. Keep the existing internal `ActiveTurn` constructors / `merge` / `finalize` calls; only the top-level flow changes. Every `ActiveTurn { ... }` literal adds `call_ids: Vec::new(),`.

```rust
    /// Ingest one completed LlmCall. The call must have been pre-identified
    /// by the upstream stage; `identity` carries the extracted session/turn ids.
    /// Returns TurnEvents in emission order. Does NOT mutate `call`.
    pub fn ingest(
        &mut self,
        call: &LlmCall,
        identity: &ts_llm::model::CallIdentity,
    ) -> Vec<TurnEvent> {
        self.virtual_now_us = self.virtual_now_us.max(
            call.complete_time.or(call.response_time).unwrap_or(call.request_time)
        );
        let profile = match self.registry.find_by_name(identity.profile_name) {
            Some(p) => p,
            None => return Vec::new(),
        };

        let mut events = Vec::new();
        let explicit_turn = identity.turn_id_hint.clone();
        let subagent = profile.subagent(call);
        let session_id = identity.session_id.clone();

        // --- Explicit turn_id path (Codex) ---
        if let Some(turn_id) = explicit_turn {
            let key = TurnKey { session_id: session_id.clone(), turn_id: turn_id.clone() };

            // Close any OTHER active turn in same session with a different turn_id.
            let stale_keys: Vec<TurnKey> = self.active.keys()
                .filter(|k| k.session_id == session_id && k.turn_id != turn_id)
                .cloned().collect();
            for sk in stale_keys {
                if let Some(at) = self.active.remove(&sk) {
                    events.push(TurnEvent::Completed(at.finalize(TurnStatus::Incomplete)));
                }
            }

            let is_new = !self.active.contains_key(&key);
            let initial_user_input = if is_new { profile.extract_user_input(call) } else { None };
            let at = self.active.entry(key.clone()).or_insert_with(|| ActiveTurn {
                key: key.clone(),
                tenant_id: call.tenant_id.clone(),
                provider: call.provider.to_string(),
                client_kind: profile.name().to_string(),
                start_time_us: call.request_time,
                last_activity_us: call.request_time,
                call_count: 0,
                call_ids: Vec::new(),
                models_used: Vec::new(),
                subagents_used: Vec::new(),
                total_input_tokens: 0, total_output_tokens: 0, total_cached_input_tokens: 0,
                last_finish_reason: None,
                user_input: initial_user_input,
                final_answer_preview: None,
                final_call_id: None,
            });
            if is_new {
                events.push(TurnEvent::Started { key: key.clone(), start_time_us: at.start_time_us });
            }
            at.merge(profile, call, subagent);  // pushes call.id into at.call_ids
            events.push(TurnEvent::CallAdded {
                key: key.clone(),
                call_id: call.id.clone(),
                sequence: at.call_count - 1,
            });
            return events;
        }

        // --- Implicit path (Anthropic) ---
        let is_user_start = profile.is_user_turn_start(call).unwrap_or(false);

        let existing_key: Option<TurnKey> = self.active.keys()
            .find(|k| k.session_id == session_id)
            .cloned();

        if let Some(ref key) = existing_key {
            let last_finish = self.active.get(key).and_then(|t| t.last_finish_reason);
            let terminal = matches!(
                last_finish,
                Some(FinishReason::Complete) | Some(FinishReason::Length) |
                Some(FinishReason::Error) | Some(FinishReason::Cancelled)
            );
            if terminal || is_user_start {
                if let Some(at) = self.active.remove(key) {
                    let status = match (terminal, last_finish) {
                        (true, Some(FinishReason::Complete)) => TurnStatus::Complete,
                        (true, Some(FinishReason::Length)) => TurnStatus::Length,
                        (true, Some(FinishReason::Cancelled)) => TurnStatus::Cancelled,
                        (true, Some(FinishReason::Error)) => TurnStatus::Failed,
                        _ => TurnStatus::Incomplete,
                    };
                    events.push(TurnEvent::Completed(at.finalize(status)));
                }
            }
        }

        let key = match self.active.keys().find(|k| k.session_id == session_id).cloned() {
            Some(k) => k,
            None => {
                let seq = self.next_turn_seq.entry(session_id.clone()).or_insert(0);
                *seq += 1;
                let short = session_id.chars().take(8).collect::<String>();
                let new_turn_id = format!("turn-{}-{}", short, seq);
                TurnKey { session_id: session_id.clone(), turn_id: new_turn_id }
            }
        };

        let is_new = !self.active.contains_key(&key);
        let initial_user_input = if is_new { profile.extract_user_input(call) } else { None };
        let at = self.active.entry(key.clone()).or_insert_with(|| ActiveTurn {
            key: key.clone(),
            tenant_id: call.tenant_id.clone(),
            provider: call.provider.to_string(),
            client_kind: profile.name().to_string(),
            start_time_us: call.request_time,
            last_activity_us: call.request_time,
            call_count: 0,
            call_ids: Vec::new(),
            models_used: Vec::new(),
            subagents_used: Vec::new(),
            total_input_tokens: 0, total_output_tokens: 0, total_cached_input_tokens: 0,
            last_finish_reason: None,
            user_input: initial_user_input,
            final_answer_preview: None,
            final_call_id: None,
        });
        if is_new {
            events.push(TurnEvent::Started { key: key.clone(), start_time_us: at.start_time_us });
        }
        at.merge(profile, call, subagent);
        events.push(TurnEvent::CallAdded {
            key: key.clone(),
            call_id: call.id.clone(),
            sequence: at.call_count - 1,
        });

        let current_finish = call.finish_reason;
        let now_terminal = matches!(
            current_finish,
            Some(FinishReason::Complete) | Some(FinishReason::Length) |
            Some(FinishReason::Error) | Some(FinishReason::Cancelled)
        );
        if now_terminal && !is_new {
            if let Some(at) = self.active.remove(&key) {
                let status = match current_finish {
                    Some(FinishReason::Complete) => TurnStatus::Complete,
                    Some(FinishReason::Length) => TurnStatus::Length,
                    Some(FinishReason::Cancelled) => TurnStatus::Cancelled,
                    Some(FinishReason::Error) => TurnStatus::Failed,
                    _ => TurnStatus::Incomplete,
                };
                events.push(TurnEvent::Completed(at.finalize(status)));
            }
        }

        events
    }
```

- [ ] **Step 5: Fix all existing `ingest(&mut c)` call sites in the tracker tests**

Every existing test in `mod tests` that calls `t.ingest(&mut c)` must now compute a `CallIdentity` and pass `&c` (immutable). Add this helper inside `mod tests`:

```rust
    fn identity_for(call: &ts_llm::model::LlmCall, profile_name: &'static str) -> ts_llm::model::CallIdentity {
        let reg = profiles::build_default_registry();
        let profile = reg.find_by_name(profile_name).expect("known profile");
        let ids = profile.extract_ids(call).expect("profile extract_ids");
        ts_llm::model::CallIdentity {
            profile_name,
            client_kind: profile_name.to_string(),
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        }
    }
```

Then update every existing ingest call. The pattern: build identity, then call `ingest(&c, &identity)`. Concrete edits for each existing test:

- `codex_same_turn_id_accumulates`: before each `t.ingest(&mut c1);`, insert `let id1 = identity_for(&c1, "codex-cli"); let e1 = t.ingest(&c1, &id1);` (and similarly for c2).
- `codex_new_turn_id_opens_new_turn_and_closes_old`: same pattern.
- `anthropic_captures_user_input_and_final_answer`, `final_answer_preview_is_truncated`, `anthropic_tool_use_keeps_turn_open`, `anthropic_end_turn_closes_and_next_user_message_opens_new_turn`, `anthropic_new_user_message_without_end_turn_closes_old_as_incomplete`, `sweep_finalizes_idle_turn_as_incomplete`, `flush_all_finalizes_every_active_turn`: for each `t.ingest(&mut c<n>)`, insert `let id<n> = identity_for(&c<n>, "claude-cli");` directly before, then change the call to `t.ingest(&c<n>, &id<n>)`.

Also **remove** any lines in these tests that previously asserted `c.turn_id == Some(...)` or `c.session_id == Some(...)` — those fields no longer exist. If a test depended on that assertion to verify turn routing, replace with an assertion on the emitted `TurnEvent::CallAdded.key.turn_id`.

(`codex_call` corresponds to `"codex-cli"` and `anthropic_call` to `"claude-cli"`.)

- [ ] **Step 6: Run tracker tests**

Run: `cargo test -p ts-turn --lib tracker::tests`
Expected: all existing tests + three new ones pass.

- [ ] **Step 7: Commit**

```bash
git add server/ts-turn/src/tracker.rs
git commit -m "refactor(ts-turn): TurnTracker::ingest takes &LlmCall + CallIdentity

ingest no longer runs registry.find() — the caller (llm-proc) has
already identified the call and passes the result. The call is now
immutable; per-turn call_ids are accumulated on ActiveTurn and flow
into LlmTurn.call_ids. Tracker looks up the profile by name for
per-profile semantics (is_user_turn_start, extract_user_input, etc.)."
```

---

## Task 7: Add `TurnStageConfig` + `spawn_turn_stage`

**Files:**
- Create: `server/ts-turn/src/stage.rs`
- Modify: `server/ts-turn/src/lib.rs` (expose new module)

The stage takes `T` receivers of `IdentifiedCall`, spawns `T` tasks each owning its own `TurnTracker`, and fans out `LlmTurn`s into a shared `turns_tx`. **The turn stage does NOT touch calls_tx** — storage gets its own Arc<LlmCall> copy directly from llm-proc. The turn shard reads `&identified.call` (immutable) to update its tracker state and emits only `LlmTurn` rows.

- [ ] **Step 1: Write failing test for single-shard stage**

Create `server/ts-turn/src/stage.rs` with:

```rust
//! Turn tracking stage: spawns T parallel TurnTracker tasks, each shard
//! keyed by hash(session_id) % T. Each shard owns its own tracker and
//! emits finalized LlmTurns to turns_tx. LlmCalls flow directly from
//! llm-proc to storage (Arc<LlmCall> shared read-only); this stage does
//! not forward them.

use std::sync::Arc;

use tokio::sync::mpsc;

use ts_llm::model::IdentifiedCall;
use ts_llm::profile::ProfileRegistry;

use crate::model::LlmTurn;
use crate::tracker::{TrackerConfig, TurnEvent, TurnTracker};

#[derive(Debug, Clone)]
pub struct TurnStageConfig {
    pub shard_count: usize,
    pub tracker: TrackerConfig,
}

impl Default for TurnStageConfig {
    fn default() -> Self {
        Self {
            shard_count: 1,
            tracker: TrackerConfig::default(),
        }
    }
}

/// Spawn T turn-tracker tasks. Panics if `shard_rxs.len() != config.shard_count`.
/// `build_registry` is called once per shard so each shard owns its own
/// `ProfileRegistry` (sidesteps `ProfileRegistry: !Clone`).
pub fn spawn_turn_stage<F>(
    config: TurnStageConfig,
    shard_rxs: Vec<mpsc::Receiver<IdentifiedCall>>,
    turns_tx: mpsc::Sender<LlmTurn>,
    build_registry: F,
)
where
    F: Fn() -> ProfileRegistry + Send + Sync + 'static,
{
    assert_eq!(
        shard_rxs.len(),
        config.shard_count,
        "spawn_turn_stage: shard_rxs.len() must equal config.shard_count",
    );
    let build_registry = Arc::new(build_registry);
    for mut rx in shard_rxs {
        let turns_tx = turns_tx.clone();
        let tracker_cfg = config.tracker.clone();
        let build = build_registry.clone();
        tokio::spawn(async move {
            let reg = build();
            let mut tracker = TurnTracker::new(reg, tracker_cfg);
            while let Some(identified) = rx.recv().await {
                // `identified.call` is Arc<LlmCall> — we pass &LlmCall (Deref).
                for ev in tracker.ingest(&identified.call, &identified.identity) {
                    if let TurnEvent::Completed(t) = ev {
                        if turns_tx.send(t).await.is_err() { return; }
                    }
                }
                for ev in tracker.sweep() {
                    if let TurnEvent::Completed(t) = ev {
                        if turns_tx.send(t).await.is_err() { return; }
                    }
                }
                // Arc<LlmCall> dropped here — storage has its own Arc clone.
            }
            for ev in tracker.flush_all() {
                if let TurnEvent::Completed(t) = ev {
                    let _ = turns_tx.send(t).await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_llm::model::{ApiType, CallIdentity, FinishReason, LlmCall, ProviderFormat};
    use ts_llm::profiles::build_default_registry;

    fn anthropic_call(session: &str, ts_us: i64, finish: FinishReason) -> LlmCall {
        let body = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"go"}]}]}"#;
        LlmCall {
            id: format!("c-{ts_us}"),
            provider: ProviderFormat::Anthropic, model: "claude".into(),
            api_type: ApiType::Chat, tenant_id: None,
            request_time: ts_us,
            response_time: Some(ts_us + 100_000),
            complete_time: Some(ts_us + 200_000),
            request_path: "/v1/messages".into(), is_stream: true,
            request_body: Some(body.to_string()),
            status_code: Some(200), finish_reason: Some(finish), response_body: None,
            input_tokens: Some(1), output_tokens: Some(1), total_tokens: Some(2),
            ttfb_ms: None, e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(), client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(), server_port: 0,
            response_id: None,
            request_headers: vec![
                ("User-Agent".into(), "claude-cli/2.1".into()),
                ("X-Claude-Code-Session-Id".into(), session.into()),
            ],
            response_headers: vec![],
        }
    }

    fn id_for(session: &str) -> CallIdentity {
        CallIdentity {
            profile_name: "claude-cli",
            client_kind: "claude-cli".into(),
            session_id: session.into(),
            turn_id_hint: None,
        }
    }

    #[tokio::test]
    async fn single_shard_produces_turn() {
        let (shard_tx, shard_rx) = mpsc::channel::<IdentifiedCall>(16);
        let (turns_tx, mut turns_rx) = mpsc::channel::<LlmTurn>(16);

        let cfg = TurnStageConfig { shard_count: 1, ..Default::default() };
        spawn_turn_stage(cfg, vec![shard_rx], turns_tx.clone(), build_default_registry);
        drop(turns_tx);

        let c1 = Arc::new(anthropic_call("S", 1_000_000, FinishReason::ToolUse));
        let c2 = Arc::new(anthropic_call("S", 2_000_000, FinishReason::Complete));
        let (id1, id2) = (c1.id.clone(), c2.id.clone());

        shard_tx.send(IdentifiedCall { call: c1, identity: id_for("S") }).await.unwrap();
        shard_tx.send(IdentifiedCall { call: c2, identity: id_for("S") }).await.unwrap();
        drop(shard_tx);

        let mut turns = Vec::new();
        while let Some(t) = turns_rx.recv().await { turns.push(t); }
        assert_eq!(turns.len(), 1, "one complete turn expected");
        assert_eq!(turns[0].call_ids, vec![id1, id2]);
    }

    #[tokio::test]
    async fn four_shards_isolate_by_session() {
        let mut shard_txs = Vec::with_capacity(4);
        let mut shard_rxs = Vec::with_capacity(4);
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel::<IdentifiedCall>(16);
            shard_txs.push(tx);
            shard_rxs.push(rx);
        }
        let (turns_tx, mut turns_rx) = mpsc::channel::<LlmTurn>(64);

        let cfg = TurnStageConfig { shard_count: 4, ..Default::default() };
        spawn_turn_stage(cfg, shard_rxs, turns_tx.clone(), build_default_registry);
        drop(turns_tx);

        for (i, tx) in shard_txs.iter().enumerate() {
            let session = format!("S{i}");
            let c1 = Arc::new(anthropic_call(&session, 1_000_000 + i as i64, FinishReason::ToolUse));
            let c2 = Arc::new(anthropic_call(&session, 2_000_000 + i as i64, FinishReason::Complete));
            tx.send(IdentifiedCall { call: c1, identity: id_for(&session) }).await.unwrap();
            tx.send(IdentifiedCall { call: c2, identity: id_for(&session) }).await.unwrap();
        }
        drop(shard_txs);

        let mut turns = Vec::new();
        while let Some(t) = turns_rx.recv().await { turns.push(t); }
        assert_eq!(turns.len(), 4, "one turn per shard");
        let sessions: std::collections::HashSet<_> = turns.iter().map(|t| t.session_id.clone()).collect();
        assert_eq!(sessions.len(), 4);
        assert!(turns.iter().all(|t| t.call_ids.len() == 2));
    }

    #[tokio::test]
    #[should_panic(expected = "shard_rxs.len() must equal config.shard_count")]
    async fn panics_on_length_mismatch() {
        let (_tx, rx) = mpsc::channel::<IdentifiedCall>(1);
        let (turns_tx, _) = mpsc::channel::<LlmTurn>(1);
        let cfg = TurnStageConfig { shard_count: 2, ..Default::default() };
        spawn_turn_stage(cfg, vec![rx], turns_tx, build_default_registry);
    }
}
```

- [ ] **Step 2: Register the module in `ts-turn/src/lib.rs`**

Edit `server/ts-turn/src/lib.rs`. Add `pub mod stage;` and export:

```rust
pub mod model;
pub mod stage;
pub mod tracker;

pub use model::{LlmTurn, TurnKey, TurnStatus};
pub use stage::{spawn_turn_stage, TurnStageConfig};
pub use ts_llm::profile::{self, ClientProfile, ExtractedIds, ProfileRegistry};
pub use ts_llm::profiles;
pub use tracker::{TrackerConfig, TurnEvent, TurnTracker};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-turn --lib stage`
Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add server/ts-turn/src/stage.rs server/ts-turn/src/lib.rs
git commit -m "feat(ts-turn): spawn_turn_stage — sharded TurnTracker tasks

Each shard owns its own TurnTracker, ingests IdentifiedCall from its
own rx, and emits LlmTurns to a shared sink. LlmCall flows directly
from llm-proc to storage as Arc<LlmCall> — turn shard only reads.
Registry is constructed per shard via a builder closure to sidestep
ProfileRegistry: !Clone."
```

---

## Task 8: Add `MetricsStageConfig` + `spawn_metrics_stage`

**Files:**
- Create: `server/ts-metrics/src/stage.rs`
- Modify: `server/ts-metrics/src/lib.rs`

Simpler than turn — no profile logic, no call forwarding. Each shard consumes `LlmEvent` and emits `LlmMetric`.

- [ ] **Step 1: Create `ts-metrics/src/stage.rs` with failing tests**

Create `server/ts-metrics/src/stage.rs`:

```rust
//! Metrics aggregation stage: spawns M parallel MetricsAggregator tasks,
//! each shard keyed by hash(provider, model, server_ip) % M. Window close
//! is purely event-timestamp driven (no wall-clock tick).

use tokio::sync::mpsc;

use ts_llm::model::LlmEvent;

use crate::aggregator::MetricsAggregator;
use crate::model::LlmMetric;

#[derive(Debug, Clone)]
pub struct MetricsStageConfig {
    pub shard_count: usize,
}

impl Default for MetricsStageConfig {
    fn default() -> Self {
        Self { shard_count: 1 }
    }
}

/// Spawn M metrics-aggregator tasks. Panics if `shard_rxs.len() != config.shard_count`.
pub fn spawn_metrics_stage(
    config: MetricsStageConfig,
    shard_rxs: Vec<mpsc::Receiver<LlmEvent>>,
    metrics_tx: mpsc::Sender<LlmMetric>,
) {
    assert_eq!(
        shard_rxs.len(),
        config.shard_count,
        "spawn_metrics_stage: shard_rxs.len() must equal config.shard_count",
    );
    for mut rx in shard_rxs {
        let metrics_tx = metrics_tx.clone();
        tokio::spawn(async move {
            let mut agg = MetricsAggregator::new();
            while let Some(event) = rx.recv().await {
                for m in agg.process(&event) {
                    if metrics_tx.send(m).await.is_err() { return; }
                }
            }
            for m in agg.flush_all() {
                let _ = metrics_tx.send(m).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use ts_llm::model::{ApiType, CallIdentity, FinishReason, LlmCall, LlmCallStart, ProviderFormat};

    fn start_event(ts_us: i64, model: &str) -> LlmEvent {
        LlmEvent::Start(LlmCallStart {
            provider: ProviderFormat::OpenAI,
            model: model.into(),
            is_stream: true,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            timestamp_us: ts_us,
        })
    }

    fn complete_event(ts_us: i64, model: &str) -> LlmEvent {
        LlmEvent::Complete {
            call: std::sync::Arc::new(LlmCall {
                id: format!("c-{ts_us}"),
                provider: ProviderFormat::OpenAI, model: model.into(),
                api_type: ApiType::Chat, tenant_id: None,
                request_time: ts_us, response_time: Some(ts_us + 100_000),
                complete_time: Some(ts_us + 200_000),
                request_path: "/v1/chat".into(), is_stream: true,
                request_body: None, status_code: Some(200),
                finish_reason: Some(FinishReason::Complete), response_body: None,
                input_tokens: Some(10), output_tokens: Some(5), total_tokens: Some(15),
                ttfb_ms: Some(100.0), e2e_latency_ms: Some(200.0),
                client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
                client_port: 12345,
                server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
                server_port: 443,
                response_id: None,
                request_headers: vec![], response_headers: vec![],
            }),
            identity: Some(CallIdentity {
                profile_name: "x", client_kind: "x".into(),
                session_id: "s".into(), turn_id_hint: None,
            }),
        }
    }

    #[tokio::test]
    async fn single_shard_produces_metrics() {
        let (tx, rx) = mpsc::channel::<LlmEvent>(16);
        let (mtx, mut mrx) = mpsc::channel::<LlmMetric>(64);

        spawn_metrics_stage(MetricsStageConfig { shard_count: 1 }, vec![rx], mtx.clone());
        drop(mtx);

        tx.send(start_event(1_000_000_000_000, "gpt-4")).await.unwrap();
        tx.send(complete_event(1_000_000_000_000, "gpt-4")).await.unwrap();
        drop(tx);

        let mut metrics = Vec::new();
        while let Some(m) = mrx.recv().await { metrics.push(m); }
        // flush_all emits 4 granularities × 4 dimensions = 16 metrics for one call.
        assert_eq!(metrics.len(), 16);
    }

    #[tokio::test]
    async fn four_shards_aggregate_independently() {
        let mut txs = Vec::with_capacity(4);
        let mut rxs = Vec::with_capacity(4);
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel::<LlmEvent>(16);
            txs.push(tx);
            rxs.push(rx);
        }
        let (mtx, mut mrx) = mpsc::channel::<LlmMetric>(256);
        spawn_metrics_stage(MetricsStageConfig { shard_count: 4 }, rxs, mtx.clone());
        drop(mtx);

        // Each shard gets one call.
        for (i, tx) in txs.iter().enumerate() {
            let ts = 1_000_000_000_000 + i as i64 * 1_000_000;
            tx.send(start_event(ts, "gpt-4")).await.unwrap();
            tx.send(complete_event(ts, "gpt-4")).await.unwrap();
        }
        drop(txs);

        let mut metrics = Vec::new();
        while let Some(m) = mrx.recv().await { metrics.push(m); }
        // Each shard emits up to 16 metrics (4 granularity × 4 dimensions) for one call.
        assert_eq!(metrics.len(), 64, "4 shards × 16 metrics each");
    }

    #[tokio::test]
    #[should_panic(expected = "shard_rxs.len() must equal config.shard_count")]
    async fn panics_on_length_mismatch() {
        let (_tx, rx) = mpsc::channel::<LlmEvent>(1);
        let (mtx, _) = mpsc::channel::<LlmMetric>(1);
        spawn_metrics_stage(MetricsStageConfig { shard_count: 3 }, vec![rx], mtx);
    }
}
```

- [ ] **Step 2: Register the module in `ts-metrics/src/lib.rs`**

Edit `server/ts-metrics/src/lib.rs`. Replace contents with:

```rust
pub mod aggregator;
pub mod bucket;
pub mod model;
pub mod stage;

pub use stage::{spawn_metrics_stage, MetricsStageConfig};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ts-metrics --lib stage`
Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add server/ts-metrics/src/stage.rs server/ts-metrics/src/lib.rs
git commit -m "feat(ts-metrics): spawn_metrics_stage — sharded aggregator tasks

Each shard owns a MetricsAggregator, consumes LlmEvent from its own rx,
and emits LlmMetric to a shared sink. Window close remains packet-time
driven (MetricsAggregator internals unchanged)."
```

---

## Task 9: Add `StorageSinkConfig` + `spawn_storage_sink_stage`

**Files:**
- Create: `server/ts-storage/src/sink.rs`
- Modify: `server/ts-storage/src/lib.rs`

Wraps the three existing `WriteBuffer` tasks (calls / turns / metrics) behind a single stage function that returns a `JoinHandle` so `main.rs` can await final drain.

**Note on `Arc<LlmCall>` ingress:** Per the design, the llm stage fans out each completed call as `Arc<LlmCall>` to three consumers (storage, turn shard, metrics shard). The storage sink therefore takes `mpsc::Receiver<Arc<LlmCall>>`. The internal `WriteBuffer<LlmCall>` surface is unchanged — the sink's calls forwarder does one `(*arc).clone()` before pushing into the buffer. This single clone at the write boundary replaces what would otherwise be three full clones at the fanout. Preserving `StorageBackend::write_calls(&[LlmCall])` keeps every backend impl untouched.

- [ ] **Step 1: Create `sink.rs` with failing test**

Create `server/ts-storage/src/sink.rs`:

```rust
//! Storage sink stage: dedicated task group that consumes records from three
//! input channels (calls, turns, metrics), batches them via WriteBuffer, and
//! persists via StorageBackend.
//!
//! Unlike other Phase-2 stages, this one returns a JoinHandle so main.rs can
//! await the final drain after the upstream cascade closes all three channels.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use ts_llm::model::LlmCall;
use ts_metrics::model::LlmMetric;
use ts_turn::LlmTurn;

use crate::backend::StorageBackend;
use crate::buffer::create_buffer;

#[derive(Debug, Clone)]
pub struct StorageSinkConfig {
    pub channel_capacity: usize,
    pub batch_size: usize,
    pub flush_interval_ms: u64,
}

impl Default for StorageSinkConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 4096,
            batch_size: 1000,
            flush_interval_ms: 1000,
        }
    }
}

/// Spawn the storage sink. Returns a JoinHandle that completes once every
/// input channel is closed and every batched record is flushed.
pub fn spawn_storage_sink_stage(
    config: StorageSinkConfig,
    calls_rx: mpsc::Receiver<Arc<LlmCall>>,
    turns_rx: mpsc::Receiver<LlmTurn>,
    metrics_rx: mpsc::Receiver<LlmMetric>,
    backend: Arc<dyn StorageBackend>,
) -> JoinHandle<()> {
    let flush_interval = Duration::from_millis(config.flush_interval_ms);

    // Forward the external mpsc receivers into the internal WriteBuffer runtime
    // by spawning three forwarders that receive from the external rx and
    // push into the per-type buffer's handle.
    let (calls_handle, calls_buffer) =
        create_buffer::<LlmCall>(config.batch_size, flush_interval, config.channel_capacity);
    let (turns_handle, turns_buffer) =
        create_buffer::<LlmTurn>(config.batch_size, flush_interval, config.channel_capacity);
    let (metrics_handle, metrics_buffer) =
        create_buffer::<LlmMetric>(config.batch_size, flush_interval, config.channel_capacity);

    let calls_storage = backend.clone();
    let calls_task = tokio::spawn(async move {
        calls_buffer
            .run(move |batch| {
                let b = calls_storage.clone();
                async move { b.write_calls(&batch).await }
            })
            .await;
    });
    let turns_storage = backend.clone();
    let turns_task = tokio::spawn(async move {
        turns_buffer
            .run(move |batch| {
                let b = turns_storage.clone();
                async move { b.write_turns(&batch).await }
            })
            .await;
    });
    let metrics_storage = backend.clone();
    let metrics_task = tokio::spawn(async move {
        metrics_buffer
            .run(move |batch| {
                let b = metrics_storage.clone();
                async move { b.write_metrics(&batch).await }
            })
            .await;
    });

    // Forwarders: external rx → buffer handle.
    let calls_fwd = {
        let h = calls_handle.clone();
        let mut rx = calls_rx;
        tokio::spawn(async move {
            // Unwrap Arc into LlmCall for the existing WriteBuffer<LlmCall>
            // surface. When the other two consumers (turn/metrics shards)
            // still hold Arc clones, try_unwrap fails and we deep-clone once.
            while let Some(arc) = rx.recv().await {
                let call = match Arc::try_unwrap(arc) {
                    Ok(c) => c,
                    Err(a) => (*a).clone(),
                };
                if h.send(call).await.is_err() { break; }
            }
        })
    };
    let turns_fwd = {
        let h = turns_handle.clone();
        let mut rx = turns_rx;
        tokio::spawn(async move {
            while let Some(t) = rx.recv().await {
                if h.send(t).await.is_err() { break; }
            }
        })
    };
    let metrics_fwd = {
        let h = metrics_handle.clone();
        let mut rx = metrics_rx;
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if h.send(m).await.is_err() { break; }
            }
        })
    };

    tokio::spawn(async move {
        // Wait for every external rx to be drained (forwarders exit).
        let _ = tokio::join!(calls_fwd, turns_fwd, metrics_fwd);
        // Drop our buffer handles so the buffer tasks finalize and drain.
        drop(calls_handle);
        drop(turns_handle);
        drop(metrics_handle);
        // Wait for the three buffer tasks to flush and exit.
        let _ = tokio::join!(calls_task, turns_task, metrics_task);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use ts_common::error::Result;

    struct CountingBackend {
        calls: Arc<AtomicUsize>,
        turns: Arc<AtomicUsize>,
        metrics: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageBackend for CountingBackend {
        async fn init(&self) -> Result<()> { Ok(()) }
        async fn write_calls(&self, batch: &[LlmCall]) -> Result<()> {
            self.calls.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
        async fn write_turns(&self, batch: &[LlmTurn]) -> Result<()> {
            self.turns.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
        async fn write_metrics(&self, batch: &[LlmMetric]) -> Result<()> {
            self.metrics.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
        // Any other StorageBackend methods required by the trait get default
        // no-op impls — if compilation fails here, add them as Ok(()) / Ok(vec![]).
    }

    #[tokio::test]
    async fn sink_drains_all_channels_and_flushes() {
        let counts = CountingBackend {
            calls: Arc::new(AtomicUsize::new(0)),
            turns: Arc::new(AtomicUsize::new(0)),
            metrics: Arc::new(AtomicUsize::new(0)),
        };
        let (calls_count, turns_count, metrics_count) =
            (counts.calls.clone(), counts.turns.clone(), counts.metrics.clone());
        let backend: Arc<dyn StorageBackend> = Arc::new(counts);

        let (calls_tx, calls_rx) = mpsc::channel::<Arc<LlmCall>>(16);
        let (turns_tx, turns_rx) = mpsc::channel::<LlmTurn>(16);
        let (metrics_tx, metrics_rx) = mpsc::channel::<LlmMetric>(16);

        let cfg = StorageSinkConfig {
            channel_capacity: 16,
            batch_size: 2,
            flush_interval_ms: 50,
        };
        let handle = spawn_storage_sink_stage(cfg, calls_rx, turns_rx, metrics_rx, backend);

        // Send a few of each.
        for i in 0..3 {
            calls_tx.send(Arc::new(dummy_call(i))).await.unwrap();
            turns_tx.send(dummy_turn(i)).await.unwrap();
            metrics_tx.send(dummy_metric(i)).await.unwrap();
        }
        drop(calls_tx);
        drop(turns_tx);
        drop(metrics_tx);

        handle.await.unwrap();
        assert_eq!(calls_count.load(Ordering::SeqCst), 3);
        assert_eq!(turns_count.load(Ordering::SeqCst), 3);
        assert_eq!(metrics_count.load(Ordering::SeqCst), 3);
    }

    fn dummy_call(i: usize) -> LlmCall {
        use std::net::IpAddr;
        use ts_llm::model::{ApiType, ProviderFormat};
        LlmCall {
            id: format!("c-{i}"),
            provider: ProviderFormat::OpenAI, model: "m".into(),
            api_type: ApiType::Chat, tenant_id: None,
            request_time: 0, response_time: None, complete_time: None,
            request_path: "/".into(), is_stream: false, request_body: None,
            status_code: None, finish_reason: None, response_body: None,
            input_tokens: None, output_tokens: None, total_tokens: None,
            ttfb_ms: None, e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(), client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(), server_port: 0,
            response_id: None,
            request_headers: vec![], response_headers: vec![],
        }
    }
    fn dummy_turn(i: usize) -> LlmTurn {
        LlmTurn {
            turn_id: format!("t-{i}"), session_id: "s".into(),
            tenant_id: None, provider: "openai".into(), client_kind: "x".into(),
            start_time_us: 0, end_time_us: 0, duration_ms: 0,
            call_count: 1, models_used: vec![], subagents_used: vec![],
            total_input_tokens: 0, total_output_tokens: 0, total_cached_input_tokens: 0,
            total_cost_usd: None,
            status: ts_turn::TurnStatus::Complete,
            final_finish_reason: None, user_input: None,
            final_answer_preview: None, final_call_id: None,
            call_ids: vec![format!("c-{i}")],
            metadata: serde_json::json!({}),
        }
    }
    fn dummy_metric(i: usize) -> LlmMetric {
        LlmMetric {
            timestamp_us: i as i64,
            granularity: "10s",
            provider: "openai".into(),
            model: "m".into(),
            server_ip: "*".into(),
            request_count: 1,
            stream_count: 0,
            error_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_tokens: 0,
            ttfb_ms_p50: 0.0, ttfb_ms_p95: 0.0, ttfb_ms_p99: 0.0,
            e2e_ms_p50: 0.0, e2e_ms_p95: 0.0, e2e_ms_p99: 0.0,
            output_tps_avg: 0.0,
            concurrency_avg: 0.0, concurrency_max: 0,
        }
    }
}
```

- [ ] **Step 2: Register the module in `ts-storage/src/lib.rs`**

Edit `server/ts-storage/src/lib.rs`. Add:

```rust
pub mod sink;
pub use sink::{spawn_storage_sink_stage, StorageSinkConfig};
```

Add this after the existing `pub use` block.

- [ ] **Step 3: Check `LlmMetric` field names in the test**

The test uses `dummy_metric` — if compile fails on a field (e.g., `concurrency_max`), open `server/ts-metrics/src/model.rs` and match the exact field names. If `LlmMetric` doesn't include every field listed, remove the mismatched lines (the test only cares about the count, not the content).

- [ ] **Step 4: Check `StorageBackend` trait surface**

The test's `CountingBackend` implements only `init` + three writers. If `StorageBackend` has more required methods (e.g., `read_calls`, `query`), you must add default no-op / empty impls. Open `server/ts-storage/src/backend.rs` and add each missing method to `CountingBackend` with the minimal implementation that returns `Ok(Default::default())`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p ts-storage --lib sink`
Expected: `sink_drains_all_channels_and_flushes` passes.

- [ ] **Step 6: Commit**

```bash
git add server/ts-storage/src/sink.rs server/ts-storage/src/lib.rs
git commit -m "feat(ts-storage): spawn_storage_sink_stage — dedicated writer task group

Bundles the three existing WriteBuffer tasks (calls/turns/metrics)
behind a single entry point that returns a JoinHandle. Completion
signals 'every upstream channel closed and every batch flushed'."
```

---

## Task 10: Full `spawn_llm_stage` with routing fan-out

**Files:**
- Modify: `server/ts-llm/src/stage.rs` (new signature, routing logic, tests)
- Modify: `server/ts-turn/tests/integration.rs` (new signature)

llm-proc now fans out each `Arc<LlmCall>` to up to three independent destinations. Per design Routing Fan-out:

| Event | To calls_tx? | To turn shard? | To metrics shard? |
|---|---|---|---|
| `LlmEvent::Start` | no | no | yes (by `hash(provider, model, server_ip) % M`) |
| `LlmEvent::Complete { identity: Some(_) }` | yes (`Arc<LlmCall>`) | yes (by `hash(session_id) % T`) | yes |
| `LlmEvent::Complete { identity: None }` | yes (`Arc<LlmCall>`) | no | yes |

Every `Complete` reaches `calls_tx` — storage sees every call regardless of profile identification. Turn-shard participation is the identity gate.

- [ ] **Step 1: Write failing tests for routing**

Edit `server/ts-llm/src/stage.rs`. Replace the existing tests module with the new one (the old single-output-channel tests are superseded — routing is the new contract). Full test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_protocol::model::{HttpRequestData, HttpResponseData, ProtocolEvent};
    use ts_protocol::net::FlowKey;

    use crate::model::{IdentifiedCall, LlmCall, ProviderFormat};
    use crate::profile::ProfileRegistry;
    use crate::profiles::build_default_registry;

    fn flow_key(port: u16) -> FlowKey {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        FlowKey::new(ip, port, ip, 8080)
    }

    fn openai_request(fk: FlowKey, ts_us: i64) -> HttpRequestData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        });
        HttpRequestData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            version: 1,
            headers: vec![
                ("authorization".to_string(), "Bearer sk-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: ts_us,
        }
    }

    fn openai_response(fk: FlowKey, ts_us: i64) -> HttpResponseData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        });
        HttpResponseData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(body.to_string()),
            first_byte_timestamp_us: ts_us + 100_000,
            complete_timestamp_us: ts_us + 200_000,
        }
    }

    fn claude_cli_request(fk: FlowKey, ts_us: i64, session: &str) -> HttpRequestData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "claude-sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        HttpRequestData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            version: 1,
            headers: vec![
                ("user-agent".to_string(), "claude-cli/2.1.98".to_string()),
                ("x-claude-code-session-id".to_string(), session.to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: ts_us,
        }
    }

    fn anthropic_response(fk: FlowKey, ts_us: i64) -> HttpResponseData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "id": "msg_01",
            "model": "claude-sonnet",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        HttpResponseData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(body.to_string()),
            first_byte_timestamp_us: ts_us + 100_000,
            complete_timestamp_us: ts_us + 200_000,
        }
    }

    #[tokio::test]
    async fn identified_call_fans_out_to_turn_shard_and_calls_tx_and_metrics() {
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<IdentifiedCall>(16);
        let (metrics_tx, mut metrics_rx) = mpsc::channel::<crate::model::LlmEvent>(16);
        let (calls_tx, mut calls_rx) = mpsc::channel::<Arc<LlmCall>>(16);

        spawn_llm_stage(
            vec![event_rx],
            vec![turn_tx],
            vec![metrics_tx],
            calls_tx,
            Arc::new(build_default_registry()),
        );

        let fk = flow_key(5000);
        event_tx.send(ProtocolEvent::HttpRequest(claude_cli_request(fk.clone(), 1_000_000, "S1"))).await.unwrap();
        event_tx.send(ProtocolEvent::HttpResponse(anthropic_response(fk, 1_000_000))).await.unwrap();
        drop(event_tx);

        // Expect: one IdentifiedCall on turn shard, one Arc<LlmCall> on calls_tx,
        // one Start + one Complete on metrics shard.
        let turn = turn_rx.recv().await.expect("turn shard should receive");
        assert_eq!(turn.identity.session_id, "S1");

        let call = calls_rx.recv().await.expect("calls_tx should receive identified call");
        assert_eq!(call.provider, ProviderFormat::Anthropic);

        let mut start = false;
        let mut complete = false;
        while let Some(ev) = metrics_rx.recv().await {
            match ev {
                crate::model::LlmEvent::Start(_) => start = true,
                crate::model::LlmEvent::Complete { .. } => complete = true,
            }
        }
        assert!(start && complete);
    }

    #[tokio::test]
    async fn unidentified_call_skips_turn_shard_still_reaches_calls_tx_and_metrics() {
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<IdentifiedCall>(16);
        let (metrics_tx, mut metrics_rx) = mpsc::channel::<crate::model::LlmEvent>(16);
        let (calls_tx, mut calls_rx) = mpsc::channel::<Arc<LlmCall>>(16);

        spawn_llm_stage(
            vec![event_rx],
            vec![turn_tx],
            vec![metrics_tx],
            calls_tx,
            Arc::new(build_default_registry()),
        );

        let fk = flow_key(5000);
        // Plain OpenAI request — no claude-cli/codex-cli signature, no profile match.
        event_tx.send(ProtocolEvent::HttpRequest(openai_request(fk.clone(), 1_000_000))).await.unwrap();
        event_tx.send(ProtocolEvent::HttpResponse(openai_response(fk, 1_000_000))).await.unwrap();
        drop(event_tx);

        // calls_tx should receive one Arc<LlmCall>.
        let call = calls_rx.recv().await.expect("calls_tx should receive");
        assert_eq!(call.provider, ProviderFormat::OpenAI);

        // turn_tx should stay empty (no identity).
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), turn_rx.recv()).await.is_err(),
            "turn shard must stay empty for unidentified calls"
        );

        // metrics should still see Start + Complete.
        let mut count = 0;
        while metrics_rx.recv().await.is_some() { count += 1; }
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn turn_shard_index_stable_by_session_id_hash() {
        // Use 4 turn shards; all calls for the same session should land on one.
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);
        let mut turn_txs = Vec::with_capacity(4);
        let mut turn_rxs = Vec::with_capacity(4);
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel::<IdentifiedCall>(16);
            turn_txs.push(tx);
            turn_rxs.push(rx);
        }
        let (metrics_tx, _metrics_rx) = mpsc::channel::<crate::model::LlmEvent>(64);
        let (calls_tx, mut _calls_rx) = mpsc::channel::<Arc<LlmCall>>(64);
        // Drain calls_rx so sends don't block when the buffer fills.
        let drain = tokio::spawn(async move { while _calls_rx.recv().await.is_some() {} });

        spawn_llm_stage(
            vec![event_rx],
            turn_txs,
            vec![metrics_tx],
            calls_tx,
            Arc::new(build_default_registry()),
        );

        // Send two claude-cli calls for session "SAME".
        let fk1 = flow_key(5000);
        event_tx.send(ProtocolEvent::HttpRequest(claude_cli_request(fk1.clone(), 1_000_000, "SAME"))).await.unwrap();
        event_tx.send(ProtocolEvent::HttpResponse(anthropic_response(fk1, 1_000_000))).await.unwrap();
        let fk2 = flow_key(5001);
        event_tx.send(ProtocolEvent::HttpRequest(claude_cli_request(fk2.clone(), 2_000_000, "SAME"))).await.unwrap();
        event_tx.send(ProtocolEvent::HttpResponse(anthropic_response(fk2, 2_000_000))).await.unwrap();
        drop(event_tx);

        // Count how many shards received any call.
        let mut non_empty = 0;
        for mut rx in turn_rxs {
            let got_any = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await.ok().flatten();
            if got_any.is_some() { non_empty += 1; }
        }
        assert_eq!(non_empty, 1, "all SAME-session calls must pin to a single shard");
        let _ = drain.await;
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ts-llm --lib stage::tests`
Expected: FAIL — `spawn_llm_stage` still has the old signature.

- [ ] **Step 3: Rewrite `spawn_llm_stage` with routing**

Edit `server/ts-llm/src/stage.rs`. Replace the function body with the fan-out version:

```rust
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use tokio::sync::mpsc;

use ts_protocol::model::ProtocolEvent;

use crate::model::{IdentifiedCall, LlmCall, LlmEvent};
use crate::processor::LlmProcessor;
use crate::profile::ProfileRegistry;

/// Spawn N parallel LLM-extraction tasks, one per input receiver. Each task
/// owns its own `LlmProcessor` (sharing the `ProfileRegistry` via `Arc`) and
/// fans out each produced event to up to three downstream destinations:
///
/// * Every `LlmEvent` (Start and Complete) → one metrics shard, chosen by
///   `hash(provider, model, server_ip) % metrics_shard_txs.len()`.
/// * Every `LlmEvent::Complete` → `calls_tx` as `Arc<LlmCall>` (every call
///   reaches storage regardless of identification).
/// * `LlmEvent::Complete` with `identity.is_some()` → one turn shard, chosen
///   by `hash(session_id) % turn_shard_txs.len()`.
pub fn spawn_llm_stage(
    event_rxs: Vec<mpsc::Receiver<ProtocolEvent>>,
    turn_shard_txs: Vec<mpsc::Sender<IdentifiedCall>>,
    metrics_shard_txs: Vec<mpsc::Sender<LlmEvent>>,
    calls_tx: mpsc::Sender<Arc<LlmCall>>,
    registry: Arc<ProfileRegistry>,
) {
    assert!(!metrics_shard_txs.is_empty(), "spawn_llm_stage: metrics_shard_txs must be non-empty");
    assert!(!turn_shard_txs.is_empty(), "spawn_llm_stage: turn_shard_txs must be non-empty");
    let turn_shard_txs = Arc::new(turn_shard_txs);
    let metrics_shard_txs = Arc::new(metrics_shard_txs);

    for mut rx in event_rxs {
        let reg = registry.clone();
        let turn_txs = turn_shard_txs.clone();
        let metrics_txs = metrics_shard_txs.clone();
        let calls_tx = calls_tx.clone();
        tokio::spawn(async move {
            let mut processor = LlmProcessor::new(reg);
            while let Some(event) = rx.recv().await {
                for llm_event in processor.process(event) {
                    // Metrics always gets every event (clones the Arc, not the call body).
                    let metrics_idx = metrics_shard_index(&llm_event, metrics_txs.len());
                    if metrics_txs[metrics_idx].send(llm_event.clone()).await.is_err() {
                        return;
                    }
                    // Complete: calls_tx always; turn shard when identified.
                    if let LlmEvent::Complete { call, identity } = llm_event {
                        if calls_tx.send(call.clone()).await.is_err() { return; }
                        if let Some(id) = identity {
                            let idx = turn_shard_index(&id.session_id, turn_txs.len());
                            let ic = IdentifiedCall { call, identity: id };
                            if turn_txs[idx].send(ic).await.is_err() { return; }
                        }
                    }
                }
            }
        });
    }
}

fn turn_shard_index(session_id: &str, n: usize) -> usize {
    let mut h = DefaultHasher::new();
    session_id.hash(&mut h);
    (h.finish() as usize) % n
}

fn metrics_shard_index(event: &LlmEvent, n: usize) -> usize {
    let (provider, model, server_ip) = match event {
        LlmEvent::Start(s) => (s.provider.to_string(), s.model.clone(), s.server_ip.to_string()),
        LlmEvent::Complete { call, .. } => (
            call.provider.to_string(),
            call.model.clone(),
            call.server_ip.to_string(),
        ),
    };
    let mut h = DefaultHasher::new();
    provider.hash(&mut h);
    model.hash(&mut h);
    server_ip.hash(&mut h);
    (h.finish() as usize) % n
}
```

- [ ] **Step 4: Run new stage tests**

Run: `cargo test -p ts-llm --lib stage::tests`
Expected: 3 routing tests pass.

- [ ] **Step 5: Update `ts-turn/tests/integration.rs` to new signature**

Edit `server/ts-turn/tests/integration.rs`. Rewrite `run_pcap` to use the new wiring. Replace the body starting at line 42 through 94 with:

```rust
    // Composition root: all boundary channels created here.
    let worker_count = 1usize;
    let queue_size = 4096usize;
    let (raw_tx, raw_rx) = mpsc::channel(queue_size);
    let (event_tx, event_rx) = mpsc::channel(queue_size);
    let (turn_shard_tx, turn_shard_rx) =
        mpsc::channel::<ts_llm::model::IdentifiedCall>(queue_size);
    let (metrics_shard_tx, metrics_shard_rx) =
        mpsc::channel::<ts_llm::model::LlmEvent>(queue_size);
    let (calls_tx, mut calls_rx) =
        mpsc::channel::<Arc<ts_llm::model::LlmCall>>(queue_size);
    let (turns_tx, mut turns_rx) = mpsc::channel::<ts_turn::LlmTurn>(queue_size);
    let (_metrics_tx, _metrics_rx) = mpsc::channel::<ts_metrics::model::LlmMetric>(queue_size);

    let cfg = ProtocolStageConfig {
        worker_count,
        ..Default::default()
    };
    spawn_protocol_stage(cfg, raw_rx, vec![event_tx], &mut metrics_sys);

    let registry = Arc::new(ts_llm::profiles::build_default_registry());
    ts_llm::spawn_llm_stage(
        vec![event_rx],
        vec![turn_shard_tx],
        vec![metrics_shard_tx],
        calls_tx,
        registry,
    );

    ts_turn::spawn_turn_stage(
        ts_turn::TurnStageConfig { shard_count: 1, tracker: TrackerConfig::default() },
        vec![turn_shard_rx],
        turns_tx,
        ts_llm::profiles::build_default_registry,
    );

    ts_metrics::spawn_metrics_stage(
        ts_metrics::MetricsStageConfig { shard_count: 1 },
        vec![metrics_shard_rx],
        _metrics_tx,
    );

    let _metrics_svc = metrics_sys.start();

    let source = PcapFileSource::new(path);
    let cancel = tokio_util::sync::CancellationToken::new();
    let src_task = tokio::spawn({
        let tx = raw_tx.clone();
        let cancel = cancel.clone();
        async move {
            let _ = Box::new(source).run(tx, source_metrics, cancel).await;
        }
    });
    drop(raw_tx);

    // Drain calls_rx in background so the llm stage does not block on backpressure.
    // We don't care about `LlmCall` content in this test — we only assert on turns.
    let calls_drain = tokio::spawn(async move {
        while calls_rx.recv().await.is_some() {}
    });

    let mut finalized: Vec<ts_turn::LlmTurn> = Vec::new();
    while let Some(turn) = turns_rx.recv().await { finalized.push(turn); }

    let _ = src_task.await;
    let _ = calls_drain.await;
    Some(finalized)
```

Remove the old `TurnTracker` / `TurnEvent` imports from the top of the file and add `use std::sync::Arc;`. Keep `use ts_turn::LlmTurn;` available (already re-exported). `ts_llm::profiles::build_default_registry` is the post-Task-1 home of the registry builder.

- [ ] **Step 6: Run integration test**

Run: `cargo test -p ts-turn --test integration`
Expected: `claude_cli_messages_expects_one_complete_turn` passes (or skips gracefully if fixture is missing — that's the intended behavior).

- [ ] **Step 7: Commit**

```bash
git add server/ts-llm/src/stage.rs server/ts-turn/tests/integration.rs
git commit -m "feat(ts-llm): spawn_llm_stage fans Arc<LlmCall> to sink/turn/metrics

Every LlmEvent reaches a metrics shard by hash(provider, model,
server_ip). Every Complete reaches calls_tx as Arc<LlmCall>. Complete
with identity is also routed to a turn shard by hash(session_id).
Three independent consumers, one immutable Arc, no forwarding chain."
```

---

## Task 11: Extend `AppConfig` — turn.shard_count, metrics, storage.sink

**Files:**
- Modify: `server/ts-common/src/config.rs`
- Modify: `server/config/default.toml` (if present)

- [ ] **Step 1: Write failing test for new config defaults**

Edit `server/ts-common/src/config.rs`. Add to the bottom:

```rust
#[cfg(test)]
mod phase2_tests {
    use super::*;

    #[test]
    fn turn_config_has_shard_count_default_1() {
        let cfg = TurnConfig::default();
        assert_eq!(cfg.shard_count, 1);
    }

    #[test]
    fn metrics_config_has_shard_count_default_1() {
        let cfg = MetricsConfig::default();
        assert_eq!(cfg.shard_count, 1);
    }

    #[test]
    fn storage_sink_config_defaults() {
        let cfg = StorageSinkConfig::default();
        assert_eq!(cfg.channel_capacity, 4096);
        assert_eq!(cfg.flush_interval_ms, 1000);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ts-common --lib phase2_tests`
Expected: FAIL — `MetricsConfig`, `StorageSinkConfig`, `TurnConfig.shard_count` don't exist.

- [ ] **Step 3: Add `shard_count` to `TurnConfig`**

Edit `server/ts-common/src/config.rs`. Modify `TurnConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TurnConfig {
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    #[serde(default = "default_turn_shard_count")]
    pub shard_count: usize,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            sweep_interval_secs: default_sweep_interval_secs(),
            shard_count: default_turn_shard_count(),
        }
    }
}

fn default_turn_shard_count() -> usize { 1 }
```

- [ ] **Step 4: Add `MetricsConfig`**

Same file. Add below `TurnConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_metrics_shard_count")]
    pub shard_count: usize,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { shard_count: default_metrics_shard_count() }
    }
}

fn default_metrics_shard_count() -> usize { 1 }
```

- [ ] **Step 5: Add `StorageSinkConfig`**

Same file. Add below `MetricsConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct StorageSinkConfig {
    #[serde(default = "default_sink_channel_capacity")]
    pub channel_capacity: usize,
    #[serde(default = "default_sink_flush_interval_ms")]
    pub flush_interval_ms: u64,
}

impl Default for StorageSinkConfig {
    fn default() -> Self {
        Self {
            channel_capacity: default_sink_channel_capacity(),
            flush_interval_ms: default_sink_flush_interval_ms(),
        }
    }
}

fn default_sink_channel_capacity() -> usize { 4096 }
fn default_sink_flush_interval_ms() -> u64 { 1000 }
```

- [ ] **Step 6: Wire into `AppConfig`**

Same file, modify `AppConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub pipeline: PipelineConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub internal_metrics: InternalMetricsConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub turn: TurnConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub storage_sink: StorageSinkConfig,
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p ts-common --lib`
Expected: all config tests pass.

- [ ] **Step 8: Update `config/default.toml` (if present)**

Run: `ls server/config/default.toml` — if it exists, append:

```toml
[metrics]
shard_count = 1

[storage_sink]
channel_capacity = 4096
flush_interval_ms = 1000

[turn]
# existing fields unchanged
shard_count = 1
```

(If a `[turn]` section already exists, add just the `shard_count = 1` line within it.)

- [ ] **Step 9: Commit**

```bash
git add server/ts-common/src/config.rs server/config/default.toml
git commit -m "feat(ts-common): add Phase-2 config — turn.shard_count, metrics, storage_sink

Defaults preserve Phase 1 behavior: T=1, M=1, sink channel 4096,
flush 1s."
```

---

## Task 12: Rewrite `main.rs` composition root

**Files:**
- Modify: `server/app/tokenscope/src/main.rs`

This task replaces the Phase-1 `tokio::select!` event loop with the Phase-2 stage wiring. The main loop becomes: (1) create all boundary channels, (2) spawn every stage, (3) launch capture sources, (4) wait on ctrl-c OR sink completion, (5) drop `raw_tx` + await sink.

- [ ] **Step 1: Replace the inner "has sources" branch of main**

Edit `server/app/tokenscope/src/main.rs`. The block from line 255 to line 443 (the `if !sources.is_empty() { ... }` branch) is rewritten. Replace that entire inner block (keeping the `if !sources.is_empty() {` guard and its closing `}`) with:

```rust
    if !sources.is_empty() {
        // Register per-source metrics workers up front.
        let capture_metrics: Vec<_> = (0..sources.len())
            .map(|i| {
                metrics_sys.register_worker(
                    &format!("capture.{i}"),
                    &[
                        Metric::CapturePacketsReceived,
                        Metric::CapturePacketsDropped,
                    ],
                )
            })
            .collect();

        // Compose channels: every cross-stage boundary lives here.
        let worker_count = config.pipeline.worker_count;
        let turn_shards = config.turn.shard_count;
        let metrics_shards = config.metrics.shard_count;
        let queue_size = 4096usize;

        let (raw_tx, raw_rx) = mpsc::channel(queue_size);

        let mut event_txs = Vec::with_capacity(worker_count);
        let mut event_rxs = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let (tx, rx) = mpsc::channel(queue_size);
            event_txs.push(tx);
            event_rxs.push(rx);
        }

        let mut turn_shard_txs = Vec::with_capacity(turn_shards);
        let mut turn_shard_rxs = Vec::with_capacity(turn_shards);
        for _ in 0..turn_shards {
            let (tx, rx) = mpsc::channel::<ts_llm::model::IdentifiedCall>(queue_size);
            turn_shard_txs.push(tx);
            turn_shard_rxs.push(rx);
        }

        let mut metrics_shard_txs = Vec::with_capacity(metrics_shards);
        let mut metrics_shard_rxs = Vec::with_capacity(metrics_shards);
        for _ in 0..metrics_shards {
            let (tx, rx) = mpsc::channel::<LlmEvent>(queue_size);
            metrics_shard_txs.push(tx);
            metrics_shard_rxs.push(rx);
        }

        let sink_capacity = config.storage_sink.channel_capacity;
        let (calls_tx, calls_rx) =
            mpsc::channel::<std::sync::Arc<LlmCall>>(sink_capacity);
        let (turns_tx, turns_rx) = mpsc::channel::<LlmTurn>(sink_capacity);
        let (metrics_out_tx, metrics_out_rx) = mpsc::channel::<LlmMetric>(sink_capacity);

        // Protocol stage.
        let protocol_cfg = ProtocolStageConfig {
            worker_count,
            ..Default::default()
        };
        spawn_protocol_stage(protocol_cfg, raw_rx, event_txs, &mut metrics_sys);

        // LLM stage (with routing fan-out to turn, metrics, calls_tx).
        let registry = std::sync::Arc::new(ts_llm::profiles::build_default_registry());
        ts_llm::spawn_llm_stage(
            event_rxs,
            turn_shard_txs,
            metrics_shard_txs,
            calls_tx.clone(),
            registry,
        );

        // Turn stage — does NOT touch calls_tx; its only output is turns_tx.
        let turn_cfg = ts_turn::TurnStageConfig {
            shard_count: turn_shards,
            tracker: TrackerConfig {
                idle_timeout_us: (config.turn.idle_timeout_secs as i64) * 1_000_000,
                sweep_interval_us: (config.turn.sweep_interval_secs as i64) * 1_000_000,
            },
        };
        ts_turn::spawn_turn_stage(
            turn_cfg,
            turn_shard_rxs,
            turns_tx,
            ts_llm::profiles::build_default_registry,
        );

        // Metrics stage.
        ts_metrics::spawn_metrics_stage(
            ts_metrics::MetricsStageConfig { shard_count: metrics_shards },
            metrics_shard_rxs,
            metrics_out_tx,
        );

        // Storage sink.
        let sink_cfg = ts_storage::StorageSinkConfig {
            channel_capacity: config.storage_sink.channel_capacity,
            batch_size: config.storage.batch_size,
            flush_interval_ms: config.storage_sink.flush_interval_ms,
        };
        let sink_handle = ts_storage::spawn_storage_sink_stage(
            sink_cfg, calls_rx, turns_rx, metrics_out_rx, storage.clone(),
        );

        // llm-stage owns the only live calls_tx clones (one per worker). When
        // upstream closes and every llm-proc exits, every Sender<Arc<LlmCall>>
        // drops and the sink's calls_rx observes None. Drop our local handle
        // so we don't keep the channel alive artificially.
        drop(calls_tx);

        let metrics_svc = metrics_sys.start();
        let _reporter_handle =
            if config.internal_metrics.enabled && config.internal_metrics.interval_secs > 0 {
                let handle = MetricsReporter::start(
                    metrics_svc,
                    Duration::from_secs(config.internal_metrics.interval_secs),
                );
                tracing::info!(
                    "internal metrics reporter started (interval={}s)",
                    config.internal_metrics.interval_secs
                );
                Some(handle)
            } else {
                None
            };

        // Spawn capture sources.
        let cancel = CancellationToken::new();
        let mut capture_tasks: JoinSet<()> = JoinSet::new();
        for (i, (source, metrics)) in sources
            .into_iter()
            .zip(capture_metrics.into_iter())
            .enumerate()
        {
            let tx = raw_tx.clone();
            let capture_cancel = cancel.clone();
            capture_tasks.spawn(async move {
                if let Err(e) = source.run(tx, metrics, capture_cancel).await {
                    tracing::error!("capture source [{i}] error: {e}");
                }
            });
        }
        drop(raw_tx);

        // Wait for ctrl-c OR sink drain (pcap natural end).
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl+C, stopping...");
                cancel.cancel();
            }
            _ = async {
                while capture_tasks.join_next().await.is_some() {}
            } => {
                tracing::debug!("all capture sources finished");
            }
        }

        // Wait for any remaining capture tasks briefly, then await sink.
        tokio::select! {
            _ = async {
                while capture_tasks.join_next().await.is_some() {}
            } => {}
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                tracing::warn!("capture source(s) did not stop in time; aborting");
                capture_tasks.abort_all();
            }
        }

        tracing::info!("waiting for storage sink to drain...");
        let _ = sink_handle.await;
        tracing::info!("storage sink drained");
    } else {
```

Note: the block inside the `else {` (no sources) path is simpler — update it next step.

- [ ] **Step 2: Simplify the "no sources" branch**

The original `else` branch (around old line 444-471) spawns write-buffer tasks and just waits for ctrl-c. With the sink stage now the single owner of write buffers, the no-sources path simplifies. Replace the `else` block with:

```rust
    } else {
        let metrics_svc = metrics_sys.start();

        let _reporter_handle =
            if config.internal_metrics.enabled && config.internal_metrics.interval_secs > 0 {
                Some(MetricsReporter::start(
                    metrics_svc,
                    Duration::from_secs(config.internal_metrics.interval_secs),
                ))
            } else {
                None
            };

        tracing::info!(
            "no capture sources configured (use --pcap-file, -i <interface>, or [[capture.sources]] in config)"
        );
        tracing::info!("tokenscope ready, press Ctrl+C to stop");

        match tokio::signal::ctrl_c().await {
            Ok(()) => tracing::info!("received Ctrl+C, shutting down..."),
            Err(e) => tracing::error!("failed to listen for Ctrl+C: {e}"),
        }
    }
```

- [ ] **Step 3: Remove now-unused imports**

At the top of `server/app/tokenscope/src/main.rs`, remove these imports (Phase 1 leftovers that the new body doesn't use):

```rust
use ts_metrics::aggregator::MetricsAggregator;  // REMOVE
use ts_storage::{create_backend, create_buffer}; // CHANGE to: use ts_storage::create_backend;
use ts_turn::profiles::build_default_registry;  // REMOVE — moved to ts_llm::profiles (Task 1); referenced inline above as a fn item
use ts_turn::tracker::{TrackerConfig, TurnEvent, TurnTracker}; // CHANGE to: use ts_turn::tracker::TrackerConfig;
```

Also ensure `use ts_llm::model::{LlmCall, LlmEvent};` is present so the channel generic types resolve. `TurnEvent` / `TurnTracker` are no longer constructed in main.rs — only `TrackerConfig` is passed into `TurnStageConfig`.

- [ ] **Step 4: Build and run**

Run: `cargo build --workspace`
Expected: compiles clean.

Run: `cargo test --workspace`
Expected: all tests pass.

- [ ] **Step 5: Smoke-test with pcap fixture (if available)**

Run: `ls server/testdata/pcaps/`. If a `claude-cli-messages.pcap` exists, run:

```bash
cargo run --manifest-path server/Cargo.toml -- --pcap-file server/testdata/pcaps/claude-cli-messages.pcap
```

Expected: program ingests the pcap, writes to `data/tokenscope.duckdb`, and exits without errors after the file is consumed.

- [ ] **Step 6: Commit**

```bash
git add server/app/tokenscope/src/main.rs
git commit -m "refactor(tokenscope): main.rs becomes pure composition root

Delete the Phase-1 tokio::select! loop owning TurnTracker +
MetricsAggregator + write-buffer handles. Main now only creates
boundary channels, spawns stages (protocol/llm/turn/metrics/sink),
launches capture sources, and awaits ctrl-c or sink completion."
```

---

## Task 13: Pcap parity assertion in integration test

**Files:**
- Modify: `server/ts-turn/tests/integration.rs` — add assertion comparing T=M=1 output vs T=4 M=4 output.

- [ ] **Step 1: Add multi-shard parity test**

Edit `server/ts-turn/tests/integration.rs`. Add a new async helper `run_pcap_sharded(name, turn_shards, metrics_shards)` that builds the same pipeline as `run_pcap` but with configurable shard counts. Add both the helper and the new test:

```rust
async fn run_pcap_sharded(
    name: &str,
    turn_shards: usize,
    metrics_shards: usize,
) -> Option<Vec<ts_turn::LlmTurn>> {
    let path = fixture(name)?;
    let mut metrics_sys = MetricsSystem::new();

    let source_metrics = metrics_sys.register_worker(
        "capture.test",
        &[
            Metric::CapturePacketsReceived,
            Metric::CapturePacketsDropped,
        ],
    );

    let worker_count = 1usize;
    let queue_size = 4096usize;
    let (raw_tx, raw_rx) = mpsc::channel(queue_size);
    let (event_tx, event_rx) = mpsc::channel(queue_size);

    let mut turn_shard_txs = Vec::with_capacity(turn_shards);
    let mut turn_shard_rxs = Vec::with_capacity(turn_shards);
    for _ in 0..turn_shards {
        let (tx, rx) = mpsc::channel::<ts_llm::model::IdentifiedCall>(queue_size);
        turn_shard_txs.push(tx);
        turn_shard_rxs.push(rx);
    }

    let mut metrics_shard_txs = Vec::with_capacity(metrics_shards);
    let mut metrics_shard_rxs = Vec::with_capacity(metrics_shards);
    for _ in 0..metrics_shards {
        let (tx, rx) = mpsc::channel::<ts_llm::model::LlmEvent>(queue_size);
        metrics_shard_txs.push(tx);
        metrics_shard_rxs.push(rx);
    }

    let (calls_tx, mut calls_rx) =
        mpsc::channel::<Arc<ts_llm::model::LlmCall>>(queue_size);
    let (turns_tx, mut turns_rx) = mpsc::channel::<ts_turn::LlmTurn>(queue_size);
    let (m_out_tx, mut _m_out_rx) = mpsc::channel::<ts_metrics::model::LlmMetric>(queue_size);

    let cfg = ProtocolStageConfig { worker_count, ..Default::default() };
    spawn_protocol_stage(cfg, raw_rx, vec![event_tx], &mut metrics_sys);

    let registry = Arc::new(ts_llm::profiles::build_default_registry());
    ts_llm::spawn_llm_stage(
        vec![event_rx],
        turn_shard_txs,
        metrics_shard_txs,
        calls_tx,
        registry,
    );

    ts_turn::spawn_turn_stage(
        ts_turn::TurnStageConfig {
            shard_count: turn_shards,
            tracker: TrackerConfig::default(),
        },
        turn_shard_rxs,
        turns_tx,
        ts_llm::profiles::build_default_registry,
    );

    ts_metrics::spawn_metrics_stage(
        ts_metrics::MetricsStageConfig { shard_count: metrics_shards },
        metrics_shard_rxs,
        m_out_tx,
    );

    let _metrics_svc = metrics_sys.start();

    let source = PcapFileSource::new(path);
    let cancel = tokio_util::sync::CancellationToken::new();
    let src_task = tokio::spawn({
        let tx = raw_tx.clone();
        let cancel = cancel.clone();
        async move {
            let _ = Box::new(source).run(tx, source_metrics, cancel).await;
        }
    });
    drop(raw_tx);

    let calls_drain = tokio::spawn(async move {
        while calls_rx.recv().await.is_some() {}
    });
    let metrics_drain = tokio::spawn(async move {
        while _m_out_rx.recv().await.is_some() {}
    });

    let mut finalized: Vec<ts_turn::LlmTurn> = Vec::new();
    while let Some(turn) = turns_rx.recv().await { finalized.push(turn); }

    let _ = src_task.await;
    let _ = calls_drain.await;
    let _ = metrics_drain.await;
    Some(finalized)
}

#[tokio::test]
async fn claude_cli_messages_multi_shard_parity() {
    let Some(single) = run_pcap_sharded("claude-cli-messages.pcap", 1, 1).await else {
        eprintln!("skip: fixture not present"); return;
    };
    let multi = run_pcap_sharded("claude-cli-messages.pcap", 4, 4).await.unwrap();

    let single_keys: std::collections::BTreeSet<_> = single.iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status)).collect();
    let multi_keys: std::collections::BTreeSet<_> = multi.iter()
        .map(|t| (t.session_id.clone(), t.call_count, t.status)).collect();
    assert_eq!(single_keys, multi_keys, "turn sets must match across shard counts");
}
```

**Note on missing `_m_out_rx` binding:** the leading underscore is intentional — the binding is captured by the `metrics_drain` task, not discarded. If `cargo build` complains about `unused_mut`, drop the `mut` since we only call `recv()` which is `&mut self` via the owned `rx` binding inside the `async move` block (Rust auto-re-borrows). If the compiler is unhappy, reintroduce `mut` inside the spawn: `let mut rx = _m_out_rx; while rx.recv().await.is_some() {}`.

- [ ] **Step 2: Factor the existing `run_pcap` to call `run_pcap_sharded(name, 1, 1)`**

Replace the existing `run_pcap` body with:

```rust
async fn run_pcap(name: &str) -> Option<Vec<ts_turn::LlmTurn>> {
    run_pcap_sharded(name, 1, 1).await
}
```

- [ ] **Step 3: Run parity test**

Run: `cargo test -p ts-turn --test integration claude_cli_messages_multi_shard_parity`
Expected: passes (or skips if fixture absent).

- [ ] **Step 4: Commit**

```bash
git add server/ts-turn/tests/integration.rs
git commit -m "test(ts-turn): pcap parity assertion — T=M=1 vs T=M=4

Replaying the same fixture under different shard counts must produce
the same set of turns (order may differ; content must match)."
```

---

## Task 14: Workspace-wide verification

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace`
Expected: every crate's tests pass.

- [ ] **Step 2: Verify no dead code / warnings**

Run: `cargo build --workspace --all-targets 2>&1 | grep -E "warning|error"`
Expected: any warning that pre-existed is fine; no new warnings introduced by Phase 2 changes should remain — if any appear, fix them (unused import, dead code, etc.).

- [ ] **Step 3: Format check**

Run: `cargo fmt --check --manifest-path server/Cargo.toml`
Expected: no diff. If there's a diff, run `cargo fmt --manifest-path server/Cargo.toml` and commit the formatting fix.

- [ ] **Step 4: Final commit (if any formatting fix applied)**

```bash
git add -A server
git commit -m "style: cargo fmt after Phase 2 refactor"
```

---

## Self-Review Checklist (ran before plan was committed)

**1. Spec coverage**

| Spec Requirement | Task |
|---|---|
| Move ProfileRegistry from ts-turn to ts-llm | Task 1 |
| Add `find_by_name` for name lookup | Task 2 |
| Slim `LlmCall` (drop `session_id` / `turn_id` / `client_kind`); add `LlmTurn.call_ids`; update DuckDB schema | Task 3 |
| Add `CallIdentity`, `IdentifiedCall` types; `LlmEvent::Complete` becomes struct variant carrying `Arc<LlmCall>` | Task 3b |
| `LlmProcessor` does profile identification upstream | Task 4 |
| `spawn_llm_stage` single-output interim step | Task 5 |
| `TurnTracker::ingest` takes `(&LlmCall, &CallIdentity)`, skips `find`, appends `call_ids` | Task 6 |
| `spawn_turn_stage` with shard_count validation, no `calls_tx` output | Task 7 |
| `spawn_metrics_stage` with shard_count validation | Task 8 |
| `spawn_storage_sink_stage` consumes `Arc<LlmCall>`, returns JoinHandle | Task 9 |
| `spawn_llm_stage` fan-out routing — every `Complete` → `calls_tx` as `Arc<LlmCall>`; identified → turn shard; every event → metrics shard | Task 10 |
| Config: `turn.shard_count`, `metrics.shard_count`, `storage_sink.*` | Task 11 |
| `main.rs` becomes composition root | Task 12 |
| Integration test uses new stages | Task 10 step 5 |
| Pcap parity assertion T=M=1 vs T=M=4 | Task 13 |
| Packet-time-driven sweep/windows (no wall-clock ticker in shards) | Task 7 / Task 8 (shard loops contain no ticker) |
| Drop cascade shutdown | Task 9 (sink handle) + Task 12 (main.rs) |
| Unidentified calls still reach `llm_calls` | Task 10 (llm-proc → calls_tx for every Complete) |
| Turn → calls back-reference via `LlmTurn.call_ids` | Task 3 + Task 6 |
| Registry relocation flagged as breaking | Task 1 (commit message + re-exports) |

**2. Placeholder scan** — No "TBD" / "implement later" / vague error handling. Every step has explicit code.

**3. Type consistency**
- `CallIdentity` uses `profile_name: &'static str` and `turn_id_hint: Option<String>` throughout (model.rs, processor.rs, stage.rs, tracker.rs, tests)
- `IdentifiedCall { call: Arc<LlmCall>, identity: CallIdentity }` field names consistent
- `LlmEvent::Complete { call: Arc<LlmCall>, identity: Option<CallIdentity> }` consistent everywhere (Task 3b, 4, 5, 6, 7, 8, 10)
- `LlmEvent::Start(LlmCallStart)` unchanged (begin-of-call marker, not Arc-wrapped)
- `calls_tx: mpsc::Sender<Arc<LlmCall>>` consistent between Task 9 (sink ingress), Task 10 (llm stage), and Task 12 (main.rs composition)
- `LlmCall` struct lacks `session_id` / `turn_id` / `client_kind` after Task 3 — no later task references them
- `LlmTurn.call_ids: Vec<String>` added in Task 3 — populated in Task 6 ingest, asserted in Task 6 test, serialized in Task 3 schema
- `spawn_turn_stage(config, shard_rxs, turns_tx, build_registry)` — no `calls_tx` param (Task 7 definition, Task 10 integration, Task 12 main.rs)
- `spawn_turn_stage` `build_registry: F` closure — consistent call sites pass `ts_llm::profiles::build_default_registry` function pointer
- `TurnStageConfig { shard_count, tracker }` field names consistent
- `StorageSinkConfig { channel_capacity, batch_size, flush_interval_ms }` matches Task 9 definition and Task 12 usage

**4. Ambiguity check** — Plan header documents the Arc fanout pattern explicitly (llm-proc writes each `Arc<LlmCall>` to three independent channels; turn stage never forwards to calls_tx). Every task ties to the concrete function / line the implementer should touch.
