# Agent Sessions UI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship an `/agent-sessions` list page + `/agent-sessions/:source_id/:session_id` detail page rendered as a conversation transcript, plus the backend tweak needed to make the transcript fetch full user/assistant text per turn.

**Architecture:**
- Backend: switch `query_session_turns` from page-based to cursor-based pagination; return a new `SessionTurnItem` carrying full `user_input` / `final_answer` (reconstructed on demand from the referenced `llm_calls` bodies via the agent profile's extractors, batched into one `IN` query per field per page).
- Frontend: a list page using inbox-style rows + `useInfiniteQuery`, and a routed detail page that renders each turn as collapsed preview / expanded `<Markdown>` body with a metadata strip as the toggle target. Reuses the existing `AgentTurnDetailPanel` slide-over for deep inspection.

**Tech Stack:** Rust (axum, duckdb-rs), TypeScript/React (react-router, TanStack Query), Tailwind CSS, shadcn/ui primitives.

**Spec:** `docs/superpowers/specs/2026-04-23-agent-sessions-ui-design.md`

**Before you start:**
- Optional worktree: `just wt add sessions-ui` (the main branch already has unrelated uncommitted work; running these tasks in a fresh worktree keeps the diff clean).
- Backend must be green (`just quality rs && just test all`) before any frontend task starts.
- Frontend dev server: `just dev console`; backend: `just dev server`.
- Verification bar: each backend task passes `cargo test -p ts-storage` (or `-p ts-api`) + `just quality rs`. Each frontend task passes `just quality ts` + manual browser check where noted.

---

## Phase 1 — Backend

### Task 1: Add cursor + new response types in `ts-storage`

**Files:**
- Modify: `server/ts-storage/src/query.rs` (add types)
- Test: `server/ts-storage/src/query.rs` (`#[cfg(test)] mod tests` at bottom)

- [ ] **Step 1: Write the failing test for cursor encode/decode roundtrip**

Add this test block at the very bottom of `server/ts-storage/src/query.rs`:

```rust
#[cfg(test)]
mod session_turns_cursor_tests {
    use super::*;

    #[test]
    fn session_turns_cursor_roundtrip() {
        let c = SessionTurnsCursor {
            start_time_us: 1_729_000_000_000_000,
            turn_id: "abc-123".to_string(),
        };
        let encoded = encode_session_turns_cursor(&c);
        let decoded = decode_session_turns_cursor(&encoded).expect("decode");
        assert_eq!(decoded.start_time_us, c.start_time_us);
        assert_eq!(decoded.turn_id, c.turn_id);
    }

    #[test]
    fn session_turns_cursor_rejects_garbage() {
        assert!(decode_session_turns_cursor("not-hex!").is_none());
        assert!(decode_session_turns_cursor("00").is_none()); // valid hex, invalid JSON
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ts-storage session_turns_cursor -- --nocapture`
Expected: FAIL — `SessionTurnsCursor` / `encode_session_turns_cursor` / `decode_session_turns_cursor` not defined.

- [ ] **Step 3: Add the cursor type + encode/decode**

Locate the existing `SessionListCursor` / `encode_session_cursor` / `decode_session_cursor` block in `query.rs` (around the `SessionListQuery` definition). Immediately after `decode_session_cursor`, add:

```rust
/// Cursor for paginating a session's turns (most-recent first).
///
/// Tuple order matches `ORDER BY start_time DESC, turn_id DESC` on the server
/// side, so comparison `(start_time, turn_id) < (?, ?)` steps through pages
/// without duplicates even when two turns share a microsecond.
#[derive(Debug, Clone)]
pub struct SessionTurnsCursor {
    pub start_time_us: i64,
    pub turn_id: String,
}

pub fn encode_session_turns_cursor(c: &SessionTurnsCursor) -> String {
    let json = serde_json::json!({ "t": c.start_time_us, "k": c.turn_id }).to_string();
    let mut out = String::with_capacity(json.len() * 2);
    for b in json.as_bytes() {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

pub fn decode_session_turns_cursor(s: &str) -> Option<SessionTurnsCursor> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let b = u8::from_str_radix(s.get(i..i + 2)?, 16).ok()?;
        bytes.push(b);
    }
    let json = std::str::from_utf8(&bytes).ok()?;
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let start_time_us = v.get("t")?.as_i64()?;
    let turn_id = v.get("k")?.as_str()?.to_string();
    Some(SessionTurnsCursor {
        start_time_us,
        turn_id,
    })
}
```

- [ ] **Step 4: Replace the existing `SessionTurnsQuery` with the cursor-based version**

Find the current struct in `query.rs`:

```rust
pub struct SessionTurnsQuery {
    pub source_id: String,
    pub session_id: String,
    pub page: u32,
    pub page_size: u32,
}
```

Replace it with:

```rust
#[derive(Debug, Clone)]
pub struct SessionTurnsQuery {
    pub source_id: String,
    pub session_id: String,
    pub cursor: Option<SessionTurnsCursor>,
    pub page_size: u32,
}
```

- [ ] **Step 5: Add the new response types**

Immediately after the `TurnsPage` struct definition in `query.rs`, add:

```rust
/// One turn row returned by the session-turns endpoint. Identical to
/// `TurnListItem` except `user_input_preview` / `final_answer_preview` are
/// replaced by full-text `user_input` / `final_answer` (server-side
/// reconstructed from the referenced call bodies, see
/// `query_session_turns` in `duckdb.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct SessionTurnItem {
    pub turn_id: String,
    pub source_id: String,
    pub session_id: String,
    pub start_time: i64,
    pub end_time: i64,
    pub duration_ms: u64,
    pub wire_api: String,
    pub agent_kind: String,
    pub primary_model: Option<String>,
    pub models_used: Vec<String>,
    pub call_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub status: String,
    pub final_finish_reason: Option<String>,
    pub user_input: Option<String>,
    pub final_answer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTurnsPage {
    pub items: Vec<SessionTurnItem>,
    /// Opaque cursor for the next page. `None` when the current page is the
    /// last one (fewer than `page_size` rows were returned).
    pub next_cursor: Option<String>,
}
```

- [ ] **Step 6: Run the tests again to verify they pass**

Run: `cargo test -p ts-storage session_turns_cursor -- --nocapture`
Expected: PASS (both tests).

- [ ] **Step 7: Commit**

```bash
git add server/ts-storage/src/query.rs
git commit -m "feat(storage): add SessionTurnsCursor + SessionTurnItem types"
```

---

### Task 2: Update the `StorageBackend` trait

**Files:**
- Modify: `server/ts-storage/src/backend.rs:72`

- [ ] **Step 1: Change the trait method return type**

In `server/ts-storage/src/backend.rs`, locate line 72:

```rust
    async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<TurnsPage>;
```

Replace with:

```rust
    async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<SessionTurnsPage>;
```

- [ ] **Step 2: Verify the workspace no longer compiles (expected — implementations are now out of sync)**

Run: `cargo check -p ts-storage`
Expected: FAIL with mismatched return type errors in `duckdb.rs` and `sink.rs`. This tells us both implementations need updating — Tasks 3 and 4.

- [ ] **Step 3: Commit as part of Task 3's batch (don't commit standalone — workspace doesn't build)**

Skip the commit here; it comes at the end of Task 3 after both backends are fixed.

---

### Task 3: Update the no-op sink implementation

**Files:**
- Modify: `server/ts-storage/src/sink.rs` (the `query_session_turns` impl, around line 294)

- [ ] **Step 1: Update the no-op return**

Locate the `query_session_turns` impl in `sink.rs` (inside the `impl StorageBackend for NoOpSink` block). Replace its body with:

```rust
        async fn query_session_turns(&self, _query: &SessionTurnsQuery) -> Result<SessionTurnsPage> {
            Ok(SessionTurnsPage { items: vec![], next_cursor: None })
        }
```

- [ ] **Step 2: Update the sink's test-module imports**

The `use ts_storage::query::{...}` import at the top of the test module (around line 181) currently pulls `TurnsPage`. Add `SessionTurnsPage` to that import list; the existing items stay. Final shape (adjust the import line to include what's already there):

```rust
use ts_storage::query::{
    CallDetail, CallsPage, CallsQuery, HttpExchangeDetail, HttpExchangesPage, HttpExchangesQuery,
    MetricsModelRow, MetricsModelsQuery, MetricsSummaryQuery, MetricsSummaryRow,
    MetricsTimeseriesQuery, MetricsTimeseriesRow, SessionDetail, SessionListQuery,
    SessionTurnsPage, SessionTurnsQuery, SessionsPage, TurnCallItem, TurnDetail, TurnsPage,
    TurnsQuery,
};
```

(Confirm which names are actually needed by reading the current file — only add `SessionTurnsPage` if it's referenced. If the existing tests don't touch `query_session_turns`, this import change may not be necessary — skip the import edit and move on.)

- [ ] **Step 3: Verify `ts-storage` still fails to compile on duckdb.rs but sink.rs is clean**

Run: `cargo check -p ts-storage 2>&1 | grep -E 'sink\.rs|duckdb\.rs' | head -5`
Expected: no `sink.rs` errors; `duckdb.rs` still has `query_session_turns` errors (fixed in Task 4).

- [ ] **Step 4: Don't commit yet** — wait until Task 4 makes the workspace green.

---

### Task 4: Rewrite `query_session_turns` in DuckDB with cursor + batch extraction

**Files:**
- Modify: `server/ts-storage/src/duckdb.rs` (the existing `query_session_turns` impl, starting around line 2649; the `extract_full_text` helper at line 340 stays; may add `extract_full_text_batch` next to it)
- Test: `server/ts-storage/src/duckdb.rs` (update `query_session_by_id_and_turns_roundtrip` at line 4404 + add a new cursor-pagination test)

- [ ] **Step 1: Read the existing `query_session_turns` and `extract_full_text` in full**

Run: `grep -n 'query_session_turns\|extract_full_text' server/ts-storage/src/duckdb.rs`
Read both bodies end-to-end. You need to understand:
- The existing SELECT shape (which `agent_turns` columns are read).
- How `ExtractKind` + the profile registry work (lines ~330–404).
- The `spawn_blocking` + `read_pool.acquire()` dance (every other `query_*` uses the same pattern).

- [ ] **Step 2: Add a batch extraction helper**

Just after the existing `fn extract_full_text(...)` in `duckdb.rs`, add:

```rust
/// Batch version of `extract_full_text`. Given a set of `(agent_kind, call_id)`
/// pairs and an `ExtractKind` that determines which body column to read, runs
/// a single `SELECT ... WHERE id IN (...)` against `llm_calls`, then applies
/// each profile's extractor to produce the final text. Returns a map keyed by
/// the input `call_id`.
///
/// - Missing call rows, unknown `wire_api`s, and extractors that decline are
///   omitted from the result (caller falls back to preview).
/// - If `call_ids` is empty, returns an empty map with zero DB work.
fn extract_full_text_batch(
    conn: &Connection,
    kind: ExtractKind,
    requests: &[(String, String)], // (agent_kind, call_id)
) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut out: HashMap<String, String> = HashMap::new();
    if requests.is_empty() {
        return out;
    }

    // Build agent_kind lookup by call_id from the input side.
    let mut agent_by_call: HashMap<&str, &str> = HashMap::new();
    for (ak, cid) in requests {
        agent_by_call.insert(cid.as_str(), ak.as_str());
    }
    let call_ids: Vec<&str> = agent_by_call.keys().copied().collect();

    let col = match kind {
        ExtractKind::User => "request_body",
        ExtractKind::Assistant => "response_body",
    };
    let placeholders = vec!["?"; call_ids.len()].join(",");
    let sql = format!(
        "SELECT id, wire_api, {col} FROM llm_calls WHERE id IN ({placeholders})"
    );

    let registry = build_default_registry();

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let params: Vec<&dyn duckdb::ToSql> = call_ids.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let Ok(mut rows) = stmt.query(duckdb::params_from_iter(params.iter().copied())) else {
        return out;
    };

    while let Ok(Some(row)) = rows.next() {
        let Ok(id): std::result::Result<String, _> = row.get(0) else { continue };
        let Ok(wire_api_stored): std::result::Result<String, _> = row.get(1) else { continue };
        let body: Option<String> = row.get(2).ok();
        let Some(wire_api) = wa::by_name(&wire_api_stored) else { continue };
        let Some(agent_kind) = agent_by_call.get(id.as_str()).copied() else { continue };
        let Some(profile) = registry.find_by_name(agent_kind) else { continue };

        let (request_body, response_body) = match kind {
            ExtractKind::User => (body, None),
            ExtractKind::Assistant => (None, body),
        };
        let call = LlmCall {
            source_id: String::new(),
            id: String::new(),
            wire_api,
            model: String::new(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: String::new(),
            is_stream: false,
            request_body,
            status_code: None,
            finish_reason: None,
            response_body,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "0.0.0.0".parse().unwrap(),
            client_port: 0,
            server_ip: "0.0.0.0".parse().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
        };
        let extracted = match kind {
            ExtractKind::User => profile.extract_user_input(&call),
            ExtractKind::Assistant => profile.extract_assistant_text(&call),
        };
        if let Some(text) = extracted {
            out.insert(id, text);
        }
    }

    out
}
```

If `ApiType` / `LlmCall` / `wa::by_name` / `ExtractKind` aren't already in scope at the top of the file, add them to the existing `use` blocks to match what `extract_full_text` uses (they are already in scope — this helper lives in the same file).

- [ ] **Step 3: Replace the body of `query_session_turns`**

Find the existing `async fn query_session_turns(...)` (around line 2649) and replace its entire body with:

```rust
    async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<SessionTurnsPage> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let page_size = query.page_size.max(1);
            let limit = (page_size as u64) + 1;

            // Cursor filter (tuple comparison). `ORDER BY start_time DESC, turn_id DESC`.
            let (cursor_sql, cursor_ts) = if let Some(c) = &query.cursor {
                let ts = us_to_timestamp(c.start_time_us);
                let sid = c.turn_id.replace('\'', "''");
                (
                    format!(" AND (start_time, turn_id) < (CAST(? AS TIMESTAMP), '{sid}')"),
                    Some(ts),
                )
            } else {
                (String::new(), None)
            };

            // Paging query. SELECT matches TurnListItem columns + preview + call_id
            // for each side so we know whether to run extraction.
            let sql = format!(
                "SELECT turn_id, source_id, session_id, \
                        epoch_ms(start_time)   AS start_ms, \
                        epoch_ms(end_time)     AS end_ms, \
                        duration_ms, wire_api, agent_kind, primary_model, \
                        models_used_json, call_count, \
                        total_input_tokens, total_output_tokens, \
                        status, final_finish_reason, \
                        user_input_preview, user_call_id, \
                        final_answer_preview, final_call_id \
                 FROM agent_turns \
                 WHERE source_id = ? AND session_id = ?{cursor_sql} \
                 ORDER BY start_time DESC, turn_id DESC \
                 LIMIT {limit}"
            );

            let mut stmt = conn.prepare(&sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare session_turns: {e}"))
            })?;

            #[allow(clippy::type_complexity)]
            let mut fetched: Vec<(
                String, String, String, i64, i64, u64, String, String, Option<String>,
                Option<String>, u32, u64, u64, String, Option<String>,
                Option<String>, Option<String>, Option<String>, Option<String>,
            )> = Vec::new();

            {
                let mut rows = match &cursor_ts {
                    Some(ts) => stmt.query(duckdb::params![query.source_id, query.session_id, ts]),
                    None => stmt.query(duckdb::params![query.source_id, query.session_id]),
                }
                .map_err(|e| AppError::Storage(format!("failed to execute session_turns: {e}")))?;

                while let Some(row) = rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    fetched.push((
                        row.get(0)?, row.get(1)?, row.get(2)?,
                        row.get(3)?, row.get(4)?, row.get(5)?,
                        row.get(6)?, row.get(7)?, row.get(8)?,
                        row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?,
                        row.get(13)?, row.get(14)?,
                        row.get(15)?, row.get(16)?,
                        row.get(17)?, row.get(18)?,
                    ));
                }
            }

            // Detect end-of-pages via fetch+1 pattern.
            let has_more = fetched.len() as u64 > page_size as u64;
            if has_more {
                fetched.truncate(page_size as usize);
            }

            // Gather call-ids that need extraction (preview ends with `…`).
            let mut need_user: Vec<(String, String)> = Vec::new();     // (agent_kind, call_id)
            let mut need_assistant: Vec<(String, String)> = Vec::new();
            for t in &fetched {
                let (agent_kind, user_preview, user_call_id, final_preview, final_call_id) =
                    (t.7.clone(), &t.15, &t.16, &t.17, &t.18);
                if let (Some(p), Some(cid)) = (user_preview, user_call_id) {
                    if p.ends_with('…') {
                        need_user.push((agent_kind.clone(), cid.clone()));
                    }
                }
                if let (Some(p), Some(cid)) = (final_preview, final_call_id) {
                    if p.ends_with('…') {
                        need_assistant.push((agent_kind, cid.clone()));
                    }
                }
            }
            let user_map = extract_full_text_batch(&conn, ExtractKind::User, &need_user);
            let asst_map = extract_full_text_batch(&conn, ExtractKind::Assistant, &need_assistant);

            let mut items: Vec<SessionTurnItem> = Vec::with_capacity(fetched.len());
            for t in fetched {
                let (
                    turn_id, source_id, session_id, start_ms, end_ms, duration_ms,
                    wire_api, agent_kind, primary_model, models_used_raw,
                    call_count, total_input_tokens, total_output_tokens,
                    status, final_finish_reason,
                    user_preview, user_call_id, final_preview, final_call_id,
                ) = t;

                let user_input = match (user_preview.as_deref(), user_call_id.as_deref()) {
                    (Some(p), _) if !p.ends_with('…') => Some(p.to_string()),
                    (_, Some(cid)) => user_map.get(cid).cloned().or(user_preview.clone()),
                    _ => None,
                };
                let final_answer = match (final_preview.as_deref(), final_call_id.as_deref()) {
                    (Some(p), _) if !p.ends_with('…') => Some(p.to_string()),
                    (_, Some(cid)) => asst_map.get(cid).cloned().or(final_preview.clone()),
                    _ => None,
                };

                items.push(SessionTurnItem {
                    turn_id,
                    source_id,
                    session_id,
                    start_time: start_ms,
                    end_time: end_ms,
                    duration_ms,
                    wire_api,
                    agent_kind,
                    primary_model,
                    models_used: parse_json_string_list(models_used_raw.as_deref()),
                    call_count,
                    total_input_tokens,
                    total_output_tokens,
                    status,
                    final_finish_reason,
                    user_input,
                    final_answer,
                });
            }

            let next_cursor = if has_more {
                items.last().map(|last| {
                    encode_session_turns_cursor(&SessionTurnsCursor {
                        start_time_us: last.start_time.saturating_mul(1000),
                        turn_id: last.turn_id.clone(),
                    })
                })
            } else {
                None
            };

            Ok(SessionTurnsPage { items, next_cursor })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
```

Notes while implementing:
- `start_time` is stored as DuckDB `TIMESTAMP`; `epoch_ms(...)` gives ms since epoch as i64. Cursor carries microseconds; we multiply back (`saturating_mul(1000)`) when building `next_cursor`. Keep units consistent with how `SessionListCursor` treats its timestamps.
- The column names (`models_used_json`, `primary_model`, etc.) must match the `agent_turns` schema defined in `duckdb.rs:~245`. Read the `CREATE TABLE` to confirm the exact column names before wiring the SELECT.
- `rows.get(N)?` is short for `row.get::<_, T>(N).map_err(...)` on this backend — other `query_*` impls have the verbose form. Keep whichever style the file already uses in neighbouring functions for consistency.

- [ ] **Step 4: Update the existing roundtrip test**

Find `query_session_by_id_and_turns_roundtrip` (around line 4404) in `duckdb.rs`. The current test calls `query_session_turns` with `page: 1, page_size: 200`. Replace that call with:

```rust
        let turns_page = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: "".to_string(),
                session_id: "SX".to_string(),
                cursor: None,
                page_size: 200,
            })
            .await
            .unwrap();
```

Update any assertions that read `turns_page.total` (no longer exists) to read `turns_page.items.len()` instead. If the test compares `next_cursor`, assert it is `None` when fewer than `page_size` rows come back.

- [ ] **Step 5: Add a new cursor-pagination test**

Add this test next to `query_session_by_id_and_turns_roundtrip` (same test module):

```rust
    #[tokio::test]
    async fn query_session_turns_cursor_pagination() {
        use ts_storage::query::{decode_session_turns_cursor, SessionTurnsQuery};

        let backend = new_test_backend().await;
        // Seed 5 turns in session "S-CURSOR" with increasing start_times.
        for i in 0..5 {
            insert_minimal_agent_turn(
                &backend,
                &format!("turn-{i}"),
                "",             // source_id
                "S-CURSOR",
                /* start_time_us */ 1_000_000 + i * 1_000_000,
                /* user_preview */  Some(format!("ask {i}")),
                /* final_preview */ Some(format!("answer {i}")),
            )
            .await;
        }

        // Page 1: newest 2 (turn-4, turn-3).
        let p1 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: "".to_string(),
                session_id: "S-CURSOR".to_string(),
                cursor: None,
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p1.items.len(), 2);
        assert_eq!(p1.items[0].turn_id, "turn-4");
        assert_eq!(p1.items[1].turn_id, "turn-3");
        let cursor1 = p1.next_cursor.expect("more pages");

        // Page 2: turn-2, turn-1.
        let p2 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: "".to_string(),
                session_id: "S-CURSOR".to_string(),
                cursor: decode_session_turns_cursor(&cursor1),
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 2);
        assert_eq!(p2.items[0].turn_id, "turn-2");
        assert_eq!(p2.items[1].turn_id, "turn-1");
        let cursor2 = p2.next_cursor.expect("more pages");

        // Page 3: turn-0, no next cursor.
        let p3 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: "".to_string(),
                session_id: "S-CURSOR".to_string(),
                cursor: decode_session_turns_cursor(&cursor2),
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p3.items.len(), 1);
        assert_eq!(p3.items[0].turn_id, "turn-0");
        assert!(p3.next_cursor.is_none());
    }
```

If `insert_minimal_agent_turn` / `new_test_backend` don't exist, look at how the existing `query_session_by_id_and_turns_roundtrip` test seeds data and reuse or inline that same setup. Do **not** spin up a new helper if the existing test uses inline fixtures — just copy the minimal write path (e.g., direct `backend.write_turns(vec![...])`) into this test.

- [ ] **Step 6: Run the storage tests**

Run: `cargo test -p ts-storage query_session_ -- --nocapture`
Expected: both `query_session_by_id_and_turns_roundtrip` and `query_session_turns_cursor_pagination` pass. No unused-import / dead-code warnings.

- [ ] **Step 7: Run the full storage test suite + quality gate**

Run: `cargo test -p ts-storage && just quality rs`
Expected: all green.

- [ ] **Step 8: Commit (covers Tasks 2 + 3 + 4)**

```bash
git add server/ts-storage/src/backend.rs server/ts-storage/src/sink.rs server/ts-storage/src/duckdb.rs
git commit -m "refactor(storage): cursor-paginate session turns + return full user/assistant text"
```

---

### Task 5: Update the `ts-api` route handler

**Files:**
- Modify: `server/ts-api/src/routes/agent_sessions.rs` (the `SessionTurnsParams` struct + `turns` handler)

- [ ] **Step 1: Replace the `SessionTurnsParams` struct**

Find in `server/ts-api/src/routes/agent_sessions.rs`:

```rust
#[derive(Debug, Deserialize)]
pub struct SessionTurnsParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}
```

Replace with:

```rust
#[derive(Debug, Deserialize)]
pub struct SessionTurnsParams {
    /// Opaque cursor from the previous page's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}
```

The `default_page` free function can stay unused momentarily — it'll be removed in the next step.

- [ ] **Step 2: Replace the `turns` handler body**

Find the `pub async fn turns(...)` at the bottom of the same file. Replace it with:

```rust
pub async fn turns(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path((source_id, session_id)): Path<(String, String)>,
    Query(params): Query<SessionTurnsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let cursor = match &params.cursor {
        Some(s) if !s.is_empty() => Some(
            decode_session_turns_cursor(s)
                .ok_or_else(|| ApiError::InvalidParam("invalid cursor".to_string()))?,
        ),
        _ => None,
    };

    let query = SessionTurnsQuery {
        source_id,
        session_id,
        cursor,
        page_size: params.page_size.clamp(1, 200),
    };
    let page = storage.query_session_turns(&query).await?;
    Ok(ApiResponse::ok(page))
}
```

- [ ] **Step 3: Update the imports at the top of the file**

At the top of `agent_sessions.rs`, the `use ts_storage::query::{...}` line currently pulls `decode_session_cursor, SessionListQuery, SessionTurnsQuery, TimeRange`. Add `decode_session_turns_cursor` to that list so Step 2 compiles. Final:

```rust
use ts_storage::query::{
    decode_session_cursor, decode_session_turns_cursor, SessionListQuery,
    SessionTurnsQuery, TimeRange,
};
```

- [ ] **Step 4: Delete the now-unused `default_page` helper**

Remove:

```rust
fn default_page() -> u32 {
    1
}
```

- [ ] **Step 5: Verify and test**

Run: `cargo check -p ts-api && cargo test -p ts-api && just quality rs`
Expected: all green. Warnings about unused imports should be zero.

- [ ] **Step 6: Smoke-test via curl**

Start the backend in one terminal (`just dev server`) against a DB that has at least one session. In another terminal:

```bash
# Find a real session from the list:
curl -s 'http://localhost:3000/api/agent-sessions?start=0&end=9999999999&page_size=5' | jq '.data.items[0]'

# Fetch turns for it (replace source/session ids):
curl -s 'http://localhost:3000/api/agent-sessions/<src>/<sess>/turns?page_size=3' | jq '.data | {count: (.items | length), has_next: (.next_cursor != null), first_user: .items[0].user_input}'
```

Expected: JSON with `count`, `has_next`, and a non-null `first_user` string (may be null for turns with no user_input). No server-side errors in the log.

- [ ] **Step 7: Commit**

```bash
git add server/ts-api/src/routes/agent_sessions.rs
git commit -m "feat(ts-api): cursor-paginate session turns endpoint"
```

---

## Phase 2 — Frontend types + hooks

### Task 6: Add TypeScript types

**Files:**
- Modify: `console/src/types/api.ts` (append at the end of the file)

- [ ] **Step 1: Append the session types**

Add this block to the end of `console/src/types/api.ts`:

```ts
// Agent session types — /api/agent-sessions

export interface SessionListItem {
  source_id: string
  session_id: string
  agent_kind: string
  /** ms since epoch — MAX(end_time) across windowed turns, the sort key */
  last_turn_at_in_window: number
  first_turn_at: number
  last_turn_at: number
  turn_count: number
  call_count: number
  total_input_tokens: number
  total_output_tokens: number
  total_cache_read_input_tokens: number
  total_cache_creation_input_tokens: number
  total_cost_usd: number | null
  first_user_input_preview: string | null
  first_user_call_id: string | null
}

export interface SessionsPage {
  items: SessionListItem[]
  /** Opaque cursor. null when the current page is the last one. */
  next_cursor: string | null
}

export interface SessionDetail {
  source_id: string
  session_id: string
  agent_kind: string
  first_turn_at: number
  last_turn_at: number
  turn_count: number
  call_count: number
  total_input_tokens: number
  total_output_tokens: number
  total_cache_read_input_tokens: number
  total_cache_creation_input_tokens: number
  total_cost_usd: number | null
  first_user_input_preview: string | null
  first_user_call_id: string | null
}

export interface SessionTurnItem {
  turn_id: string
  source_id: string
  session_id: string
  start_time: number
  end_time: number
  duration_ms: number
  wire_api: string
  agent_kind: string
  primary_model: string | null
  models_used: string[]
  call_count: number
  total_input_tokens: number
  total_output_tokens: number
  status: string
  final_finish_reason: string | null
  /** Full text. Frontend truncates for collapsed preview (~120 chars). */
  user_input: string | null
  /** Full text. Null when the turn ended without a final answer. */
  final_answer: string | null
}

export interface SessionTurnsPage {
  items: SessionTurnItem[]
  next_cursor: string | null
}
```

- [ ] **Step 2: Typecheck**

Run: `just quality ts`
Expected: no TS errors. No new lint warnings.

- [ ] **Step 3: Commit**

```bash
git add console/src/types/api.ts
git commit -m "feat(console): add agent-session API types"
```

---

### Task 7: Add `use-agent-sessions` hooks

**Files:**
- Create: `console/src/hooks/use-agent-sessions.ts`

- [ ] **Step 1: Write the hook file**

Create `console/src/hooks/use-agent-sessions.ts`:

```ts
import { useInfiniteQuery, useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { useToolbarStore } from "@/stores/toolbar"
import type {
  SessionsPage,
  SessionDetail,
  SessionTurnsPage,
} from "@/types/api"

interface UseAgentSessionsParams {
  sourceId?: string
  /** CSV of agent kinds, e.g. "claude-cli,codex-cli" */
  agentKind?: string
  pageSize?: number
}

const DEFAULT_PAGE_SIZE = 50

export function useAgentSessions({ sourceId, agentKind, pageSize = DEFAULT_PAGE_SIZE }: UseAgentSessionsParams) {
  const start = useToolbarStore((s) => s.start)
  const end = useToolbarStore((s) => s.end)

  return useInfiniteQuery({
    queryKey: ["agent-sessions", { start, end, sourceId, agentKind, pageSize }],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) =>
      apiFetch<SessionsPage>("/api/agent-sessions", {
        start,
        end,
        page_size: pageSize,
        source_id: sourceId || undefined,
        agent_kind: agentKind || undefined,
        cursor: pageParam || undefined,
      }),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  })
}

export function useAgentSessionDetail(sourceId: string | null, sessionId: string | null) {
  return useQuery({
    queryKey: ["agent-session-detail", sourceId, sessionId],
    queryFn: () =>
      apiFetch<SessionDetail>(
        `/api/agent-sessions/${encodeURIComponent(sourceId!)}/${encodeURIComponent(sessionId!)}`,
      ),
    enabled: sourceId != null && sessionId != null,
  })
}

export function useSessionTurns(
  sourceId: string | null,
  sessionId: string | null,
  pageSize = DEFAULT_PAGE_SIZE,
) {
  return useInfiniteQuery({
    queryKey: ["session-turns", sourceId, sessionId, pageSize],
    enabled: sourceId != null && sessionId != null,
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) =>
      apiFetch<SessionTurnsPage>(
        `/api/agent-sessions/${encodeURIComponent(sourceId!)}/${encodeURIComponent(sessionId!)}/turns`,
        {
          page_size: pageSize,
          cursor: pageParam || undefined,
        },
      ),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  })
}
```

- [ ] **Step 2: Verify `apiFetch` supports a null `source_id` path segment**

Empty-string `source_id` is legal in this system (it's the default source). `encodeURIComponent("")` gives `""`, so the URL would become `/api/agent-sessions//...`. Check what the backend accepts by re-reading the route registration in `server/ts-api/src/lib.rs` — it uses `{source_id}` as a path param. Empty path segments are valid in axum path extraction; confirm via the smoke-test step in Task 5 by using a session whose `source_id` is `""`.

If empty source_id breaks routing in practice, change the FE to use a sentinel like `_` — but don't add this workaround speculatively. Let typecheck + manual browser testing reveal it.

- [ ] **Step 3: Typecheck**

Run: `just quality ts`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add console/src/hooks/use-agent-sessions.ts
git commit -m "feat(console): add agent-session data hooks"
```

---

## Phase 3 — Frontend: session list page

### Task 8: Build the `/agent-sessions` list page

**Files:**
- Create: `console/src/pages/agent-sessions.tsx`

- [ ] **Step 1: Write the page**

Create `console/src/pages/agent-sessions.tsx`:

```tsx
import { useState } from "react"
import { Link, useSearchParams } from "react-router"
import { Loader2, Filter } from "lucide-react"
import { cn } from "@/lib/utils"
import { useAgentSessions } from "@/hooks/use-agent-sessions"
import { formatNumber, formatRelativeTime, formatDuration } from "@/lib/format"
import { FilterDropdown } from "@/components/ui/filter-dropdown"
import { AgentBadge } from "@/components/ui/agent-badge"
import type { SessionListItem } from "@/types/api"

const AGENT_KIND_OPTIONS = ["claude-cli", "codex-cli"]

function SessionRow({ item }: { item: SessionListItem }) {
  const [searchParams] = useSearchParams()
  const qs = searchParams.toString()
  const href = `/agent-sessions/${encodeURIComponent(item.source_id)}/${encodeURIComponent(item.session_id)}${qs ? `?${qs}` : ""}`

  const preview = item.first_user_input_preview ?? "(no user message)"
  const cost = item.total_cost_usd != null ? `$${item.total_cost_usd.toFixed(2)}` : null
  const durationMs = item.last_turn_at - item.first_turn_at

  return (
    <Link
      to={href}
      className="block border-b border-border/50 px-4 py-3 transition-colors hover:bg-muted/40"
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <AgentBadge agentKind={item.agent_kind} />
            <span className="font-mono text-xs text-muted-foreground">
              {item.session_id.slice(0, 12)}…
            </span>
          </div>
          <div className="mt-1 truncate text-sm text-foreground">{preview}</div>
          <div className="mt-1 text-xs text-muted-foreground">
            {item.turn_count} turns · {item.call_count} calls ·{" "}
            {formatNumber(item.total_input_tokens + item.total_output_tokens)} tok
            {cost ? ` · ${cost}` : ""}
          </div>
        </div>
        <div className="shrink-0 text-right text-xs text-muted-foreground">
          <div>{formatRelativeTime(item.last_turn_at_in_window)}</div>
          <div className="text-[11px] opacity-70">{formatDuration(durationMs)}</div>
        </div>
      </div>
    </Link>
  )
}

export function AgentSessionsPage() {
  const [sourceFilter, setSourceFilter] = useState<string[]>([])
  const [agentKindFilter, setAgentKindFilter] = useState<string[]>([])

  const { data, isLoading, isError, error, fetchNextPage, hasNextPage, isFetchingNextPage } =
    useAgentSessions({
      sourceId: sourceFilter[0],
      agentKind: agentKindFilter.join(","),
    })

  const items: SessionListItem[] = data?.pages.flatMap((p) => p.items) ?? []

  return (
    <div className="flex h-full flex-col">
      {/* Page filter strip */}
      <div className="flex shrink-0 items-center gap-2 border-b border-border px-4 py-2">
        <Filter className="size-3.5 text-muted-foreground" />
        <span className="text-xs text-muted-foreground">Filters:</span>
        <FilterDropdown
          label="Source"
          options={[]}
          selected={sourceFilter}
          onChange={setSourceFilter}
        />
        <FilterDropdown
          label="Agent kind"
          options={AGENT_KIND_OPTIONS}
          selected={agentKindFilter}
          onChange={setAgentKindFilter}
        />
      </div>

      {/* Rows */}
      <div className="flex-1 overflow-auto">
        {isLoading && items.length === 0 ? (
          <div className="flex h-60 items-center justify-center">
            <Loader2 className="size-5 animate-spin text-muted-foreground" />
          </div>
        ) : isError ? (
          <div className="flex h-60 items-center justify-center text-sm text-destructive">
            Failed to load sessions: {error?.message}
          </div>
        ) : items.length === 0 ? (
          <div className="flex h-60 items-center justify-center text-sm text-muted-foreground">
            No sessions found in the selected time range
          </div>
        ) : (
          items.map((item) => (
            <SessionRow key={`${item.source_id}/${item.session_id}`} item={item} />
          ))
        )}
      </div>

      {/* Load more */}
      {hasNextPage && (
        <div className="shrink-0 border-t border-border py-3 text-center">
          <button
            onClick={() => fetchNextPage()}
            disabled={isFetchingNextPage}
            className={cn(
              "rounded border border-border bg-background px-4 py-1.5 text-sm text-muted-foreground transition-colors",
              !isFetchingNextPage && "hover:bg-muted hover:text-foreground",
            )}
          >
            {isFetchingNextPage ? (
              <span className="inline-flex items-center gap-2">
                <Loader2 className="size-3.5 animate-spin" /> Loading…
              </span>
            ) : (
              "Load more"
            )}
          </button>
        </div>
      )}
    </div>
  )
}
```

- [ ] **Step 2: Check which helpers already exist and stub/create the missing ones**

This page imports:
- `formatNumber`, `formatDuration` — already exist in `@/lib/format`.
- `formatRelativeTime` — confirm via `grep -n 'formatRelativeTime' console/src/lib/format.ts`. If it doesn't exist, add it to `@/lib/format`:

  ```ts
  /** ms-since-epoch → "16:30", "yesterday", "3d ago" */
  export function formatRelativeTime(ms: number): string {
    const now = Date.now()
    const diffMs = now - ms
    const day = 86_400_000
    if (diffMs < day && new Date(ms).toDateString() === new Date(now).toDateString()) {
      const d = new Date(ms)
      return `${d.getHours().toString().padStart(2, "0")}:${d.getMinutes().toString().padStart(2, "0")}`
    }
    if (diffMs < 2 * day) return "yesterday"
    const days = Math.floor(diffMs / day)
    return `${days}d ago`
  }
  ```

- `FilterDropdown` — already exists (used on the Agent Turns page).
- `AgentBadge` — confirm via `grep -rn 'AgentBadge' console/src/components/ui/`. If it doesn't exist, create `console/src/components/ui/agent-badge.tsx`:

  ```tsx
  import { cn } from "@/lib/utils"

  const COLOUR_BY_AGENT: Record<string, string> = {
    "claude-cli": "bg-emerald-500/20 text-emerald-700 dark:text-emerald-300 border-emerald-700/30",
    "codex-cli":  "bg-orange-500/20 text-orange-700 dark:text-orange-300 border-orange-700/30",
  }

  export function AgentBadge({ agentKind }: { agentKind: string }) {
    const palette = COLOUR_BY_AGENT[agentKind] ?? "bg-muted text-muted-foreground border-border"
    return (
      <span className={cn("inline-flex items-center rounded border px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide", palette)}>
        {agentKind}
      </span>
    )
  }
  ```

- [ ] **Step 3: Populate Source filter options**

The `FilterDropdown` for Source needs the list of known sources. Check whether an existing hook like `useFilterValues` (in `console/src/hooks/use-filter-values.ts`) exposes sources. If yes, call it and feed the values in. If no — grep for how the top toolbar populates its own source dropdown and follow that pattern. Do not add a new `/api/filters` call; reuse whatever exists.

If no source enumeration exists, leave the options empty and the dropdown will simply not list any — the filter is unused until sources come online. Make a note in the PR that source-filter population is a follow-up if this is the case.

- [ ] **Step 4: Typecheck + lint**

Run: `just quality ts`
Expected: no errors.

- [ ] **Step 5: Commit**

```bash
git add console/src/pages/agent-sessions.tsx console/src/lib/format.ts console/src/components/ui/agent-badge.tsx
git commit -m "feat(console): add /agent-sessions list page"
```

---

### Task 9: Wire the route + sidebar entry + manual verification

**Files:**
- Modify: `console/src/app.tsx` (register `/agent-sessions`)
- Modify: `console/src/components/layout/sidebar.tsx` (add nav item)

- [ ] **Step 1: Register the route**

In `console/src/app.tsx`, import the page and add the route above the existing `agent-turns` route:

```tsx
import { AgentSessionsPage } from "@/pages/agent-sessions"
```

and inside `<Routes>` (same indentation as siblings):

```tsx
<Route path="/agent-sessions" element={<AgentSessionsPage />} />
```

- [ ] **Step 2: Add the sidebar nav item**

In `console/src/components/layout/sidebar.tsx`, add an import line at the top:

```tsx
import { ..., MessageSquare, ... } from "lucide-react"
```

(pick an icon that's clearly distinct from `MessagesSquare` used for Agent Turns — `MessageSquare` or `MessageCircle` work; confirm they're visually distinguishable at 16px).

Then in the `navItems` array, insert **above** the `/agent-turns` entry:

```tsx
{ to: "/agent-sessions", icon: MessageSquare, label: "Agent Sessions" },
```

- [ ] **Step 3: Typecheck + lint**

Run: `just quality ts`

- [ ] **Step 4: Manual browser check**

Start both servers (`just dev server` + `just dev console`), open `http://localhost:5173/agent-sessions`. Verify:
- Nav item appears above "Agent Turns".
- Clicking it loads the page.
- Rows render with badge, truncated session id, preview, stats, relative time.
- Filter dropdowns open (may have no options yet — fine).
- "Load more" appears when > pageSize sessions exist; clicking it appends rows.
- Hovering a row shows the pointer cursor; clicking navigates to `/agent-sessions/:source/:session` (404/blank for now — detail page comes next).

Run: open the URL, click around, confirm. Report any anomaly as part of this task rather than proceeding.

- [ ] **Step 5: Commit**

```bash
git add console/src/app.tsx console/src/components/layout/sidebar.tsx
git commit -m "feat(console): route + sidebar entry for agent sessions"
```

---

## Phase 4 — Frontend: session detail page

### Task 10: Build the session-detail subcomponents

**Files:**
- Create: `console/src/components/session-detail/index.ts`
- Create: `console/src/components/session-detail/session-header.tsx`
- Create: `console/src/components/session-detail/turn-metadata-strip.tsx`
- Create: `console/src/components/session-detail/turn-block.tsx`

- [ ] **Step 1: Write `session-header.tsx`**

```tsx
// console/src/components/session-detail/session-header.tsx
import { AgentBadge } from "@/components/ui/agent-badge"
import { formatNumber, formatDuration } from "@/lib/format"
import type { SessionDetail } from "@/types/api"

export function SessionHeader({ detail }: { detail: SessionDetail }) {
  const cost = detail.total_cost_usd != null ? `$${detail.total_cost_usd.toFixed(2)}` : null
  const tokens = formatNumber(detail.total_input_tokens + detail.total_output_tokens)
  const duration = formatDuration(detail.last_turn_at - detail.first_turn_at)

  return (
    <div className="flex items-center gap-3 rounded-md border border-border bg-muted/30 px-3 py-2">
      <AgentBadge agentKind={detail.agent_kind} />
      <span className="font-mono text-xs text-muted-foreground">{detail.session_id}</span>
      <span className="text-xs text-muted-foreground">source: {detail.source_id || "(default)"}</span>
      <span className="flex-1" />
      <span className="text-xs text-muted-foreground">
        {detail.turn_count} turns · {detail.call_count} calls · {tokens} tok
        {cost ? ` · ${cost}` : ""} · {duration}
      </span>
    </div>
  )
}
```

- [ ] **Step 2: Write `turn-metadata-strip.tsx`**

```tsx
// console/src/components/session-detail/turn-metadata-strip.tsx
import { ChevronDown, ChevronUp } from "lucide-react"
import { TurnStatusBadge } from "@/components/ui/turn-status-badge"
import { formatDuration, formatNumber } from "@/lib/format"
import type { SessionTurnItem } from "@/types/api"

export function TurnMetadataStrip({
  turn,
  expanded,
  onToggle,
  onInspect,
}: {
  turn: SessionTurnItem
  expanded: boolean
  onToggle: () => void
  onInspect?: (turnId: string) => void
}) {
  const tokensIn = formatNumber(turn.total_input_tokens)
  const tokensOut = formatNumber(turn.total_output_tokens)

  return (
    <div
      className="ml-[60px] flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-muted/50 hover:text-foreground"
      onClick={onToggle}
    >
      {expanded ? <ChevronUp className="size-3" /> : <ChevronDown className="size-3" />}
      <TurnStatusBadge status={turn.status} />
      <span>
        {formatDuration(turn.duration_ms)} · {turn.call_count} calls · {tokensIn} in / {tokensOut} out
      </span>
      <span className="flex-1" />
      {expanded && onInspect && (
        <button
          onClick={(e) => {
            e.stopPropagation()
            onInspect(turn.turn_id)
          }}
          className="text-primary hover:underline"
        >
          View turn detail →
        </button>
      )}
    </div>
  )
}
```

- [ ] **Step 3: Write `turn-block.tsx`**

```tsx
// console/src/components/session-detail/turn-block.tsx
import { cn } from "@/lib/utils"
import { Markdown } from "@/components/ui/markdown"
import { formatDateTimeMs, formatTime } from "@/lib/format"
import { TurnMetadataStrip } from "./turn-metadata-strip"
import type { SessionTurnItem } from "@/types/api"

const PREVIEW_CHARS = 120

function preview(text: string | null): string {
  if (!text) return ""
  const trimmed = text.trim().split("\n")[0] ?? ""
  return trimmed.length > PREVIEW_CHARS ? trimmed.slice(0, PREVIEW_CHARS) + "…" : trimmed
}

export function TurnBlock({
  turn,
  expanded,
  onToggle,
  onInspect,
}: {
  turn: SessionTurnItem
  expanded: boolean
  onToggle: () => void
  onInspect: (turnId: string) => void
}) {
  const hasFinalAnswer = turn.final_answer != null && turn.final_answer.length > 0

  return (
    <div className="mb-4">
      {/* USER */}
      <div className="flex items-start gap-3">
        <div className="w-[56px] shrink-0 pt-1 text-right text-xs text-muted-foreground">
          {formatTime(turn.start_time)}
        </div>
        <div className="flex-1 rounded-r border-l-2 border-blue-400 bg-blue-50/60 px-3 py-2 dark:border-blue-500 dark:bg-blue-950/30">
          <div className="text-[10px] font-semibold uppercase tracking-wide text-blue-600 dark:text-blue-300">
            👤 User{expanded ? ` · ${formatDateTimeMs(turn.start_time)}` : ""}
          </div>
          <div className={cn("mt-1 text-sm text-foreground", !expanded && "truncate")}>
            {expanded ? <Markdown text={turn.user_input ?? ""} /> : preview(turn.user_input)}
          </div>
        </div>
      </div>

      {/* ASSISTANT */}
      <div className="mt-1 flex items-start gap-3">
        <div className="w-[56px] shrink-0" />
        <div
          className={cn(
            "flex-1 rounded-r border-l-2 px-3 py-2",
            hasFinalAnswer
              ? "border-emerald-400 bg-emerald-50/60 dark:border-emerald-500 dark:bg-emerald-950/30"
              : "border-red-400 bg-red-50/60 dark:border-red-500 dark:bg-red-950/30",
          )}
        >
          <div
            className={cn(
              "text-[10px] font-semibold uppercase tracking-wide",
              hasFinalAnswer
                ? "text-emerald-700 dark:text-emerald-300"
                : "text-red-700 dark:text-red-300",
            )}
          >
            🎯 Assistant{!hasFinalAnswer ? " · incomplete" : ""}
          </div>
          <div
            className={cn(
              "mt-1 text-sm",
              !hasFinalAnswer && "italic text-muted-foreground",
              !expanded && "truncate",
            )}
          >
            {hasFinalAnswer ? (
              expanded ? (
                <Markdown text={turn.final_answer ?? ""} />
              ) : (
                preview(turn.final_answer)
              )
            ) : (
              "Turn ended without a final answer"
            )}
          </div>
        </div>
      </div>

      <TurnMetadataStrip turn={turn} expanded={expanded} onToggle={onToggle} onInspect={onInspect} />
    </div>
  )
}
```

- [ ] **Step 4: Write the barrel file `index.ts`**

```ts
// console/src/components/session-detail/index.ts
export { SessionHeader } from "./session-header"
export { TurnBlock } from "./turn-block"
export { TurnMetadataStrip } from "./turn-metadata-strip"
```

- [ ] **Step 5: Verify referenced helpers exist**

Run: `grep -n 'formatTime\|formatDateTimeMs' console/src/lib/format.ts`
Expected: both exist (they're used on the existing turn-detail page).

Run: `grep -n 'export' console/src/components/ui/markdown.tsx && grep -n 'TurnStatusBadge' console/src/components/ui/turn-status-badge.tsx`
Expected: both components exported. If any are missing, inspect the turn-detail page to see the actual import path and adjust.

- [ ] **Step 6: Typecheck**

Run: `just quality ts`

- [ ] **Step 7: Commit**

```bash
git add console/src/components/session-detail
git commit -m "feat(console): add session-detail subcomponents"
```

---

### Task 11: Build the session detail page

**Files:**
- Create: `console/src/pages/agent-session-detail.tsx`

- [ ] **Step 1: Write the page**

```tsx
// console/src/pages/agent-session-detail.tsx
import { useCallback, useState } from "react"
import { Link, useParams } from "react-router"
import { ArrowLeft, Loader2 } from "lucide-react"
import { useAgentSessionDetail, useSessionTurns } from "@/hooks/use-agent-sessions"
import { SessionHeader, TurnBlock } from "@/components/session-detail"
import { AgentTurnDetailPanel } from "@/pages/agent-turn-detail-panel"

export function AgentSessionDetailPage() {
  const { source_id = "", session_id = "" } = useParams()
  const { data: detail, isLoading: loadingDetail, isError: errorDetail } =
    useAgentSessionDetail(source_id, session_id)
  const {
    data: turnsData,
    isLoading: loadingTurns,
    isError: errorTurns,
    fetchNextPage,
    hasNextPage,
    isFetchingNextPage,
  } = useSessionTurns(source_id, session_id)

  const [expandedTurns, setExpandedTurns] = useState<Set<string>>(new Set())
  const [selectedTurnId, setSelectedTurnId] = useState<string | null>(null)

  const toggleTurn = useCallback((turnId: string) => {
    setExpandedTurns((prev) => {
      const next = new Set(prev)
      if (next.has(turnId)) next.delete(turnId)
      else next.add(turnId)
      return next
    })
  }, [])

  if (loadingDetail && !detail) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-5 animate-spin text-muted-foreground" />
      </div>
    )
  }
  if (errorDetail || !detail) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 text-sm text-destructive">
        <span>Session not found</span>
        <Link
          to="/agent-sessions"
          className="rounded border border-border px-3 py-1 text-muted-foreground hover:bg-muted"
        >
          Back to sessions
        </Link>
      </div>
    )
  }

  const turns = turnsData?.pages.flatMap((p) => p.items) ?? []

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="shrink-0 border-b border-border px-4 py-3">
        <Link
          to="/agent-sessions"
          className="mb-2 inline-flex items-center gap-1 text-xs text-primary hover:underline"
        >
          <ArrowLeft className="size-3" /> Agent Sessions
        </Link>
        <SessionHeader detail={detail} />
      </div>

      <div className="flex-1 overflow-auto px-4 py-4">
        {loadingTurns && turns.length === 0 ? (
          <div className="flex h-40 items-center justify-center">
            <Loader2 className="size-5 animate-spin text-muted-foreground" />
          </div>
        ) : errorTurns ? (
          <div className="py-10 text-center text-sm text-destructive">Failed to load turns</div>
        ) : turns.length === 0 ? (
          <div className="py-10 text-center text-sm text-muted-foreground">No turns in this session</div>
        ) : (
          turns.map((t) => (
            <TurnBlock
              key={t.turn_id}
              turn={t}
              expanded={expandedTurns.has(t.turn_id)}
              onToggle={() => toggleTurn(t.turn_id)}
              onInspect={(id) => setSelectedTurnId(id)}
            />
          ))
        )}

        {hasNextPage && (
          <div className="pt-4 text-center">
            <button
              onClick={() => fetchNextPage()}
              disabled={isFetchingNextPage}
              className="rounded border border-border bg-background px-4 py-1.5 text-sm text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              {isFetchingNextPage ? "Loading…" : "Load older turns"}
            </button>
          </div>
        )}
      </div>

      {selectedTurnId && (
        <AgentTurnDetailPanel
          id={selectedTurnId}
          onClose={() => setSelectedTurnId(null)}
        />
      )}
    </div>
  )
}
```

- [ ] **Step 2: Typecheck**

Run: `just quality ts`

- [ ] **Step 3: Register the route**

In `console/src/app.tsx`, add the detail route immediately after the list route:

```tsx
import { AgentSessionDetailPage } from "@/pages/agent-session-detail"

// inside <Routes>:
<Route path="/agent-sessions/:source_id/:session_id" element={<AgentSessionDetailPage />} />
```

- [ ] **Step 4: Manual browser check**

With both dev servers running, click a session from the list and verify:
- URL is `/agent-sessions/<src>/<sess>`.
- Header strip shows agent badge, session id, source, stats line.
- Turn rows render as two collapsed lines (user/assistant preview) with metadata strip beneath.
- Clicking a metadata strip toggles expansion — full markdown content renders.
- Multiple turns can be expanded simultaneously.
- Expanding an incomplete turn shows "Turn ended without a final answer" on a red-bordered card.
- "View turn detail →" in an expanded card opens the existing slide-over panel.
- Closing the slide-over returns to the session transcript unchanged.
- "← Agent Sessions" returns to the list with filter state preserved.
- "Load older turns" appears when paging boundary is hit; clicking it appends older turns.

- [ ] **Step 5: Commit**

```bash
git add console/src/pages/agent-session-detail.tsx console/src/app.tsx
git commit -m "feat(console): add session detail transcript page"
```

---

## Phase 5 — Toolbar behavior

### Task 12: Hide global filter chips on session pages

**Files:**
- Modify: `console/src/components/layout/toolbar.tsx`

- [ ] **Step 1: Read the current toolbar**

Run: `sed -n '1,120p' console/src/components/layout/toolbar.tsx`
Identify where the `wire_api`, `model`, `server_ip` chips are rendered. They're likely three similar `FilterDropdown` / chip components inside a flex row.

- [ ] **Step 2: Gate the three chips behind a path check**

Import `useLocation` from `react-router` and introduce a derived boolean. Wrap the three chips' JSX in a conditional that hides them on session pages:

```tsx
import { useLocation } from "react-router"
// …inside the component:
const { pathname } = useLocation()
const hideDimensionFilters = pathname.startsWith("/agent-sessions")

// …where the three chips render:
{!hideDimensionFilters && (
  <>
    {/* existing <WireApi chip>, <Model chip>, <ServerIp chip> JSX stays */}
  </>
)}
```

Leave the time preset, refresh button, and any unrelated controls outside this conditional — they stay visible on session pages.

- [ ] **Step 3: Typecheck**

Run: `just quality ts`

- [ ] **Step 4: Manual browser check**

Navigate between `/` (Overview) and `/agent-sessions`. Confirm the three chips appear on Overview/Turns/Calls pages and disappear on both session pages (list + detail). Time preset and refresh remain visible on all pages.

- [ ] **Step 5: Commit**

```bash
git add console/src/components/layout/toolbar.tsx
git commit -m "feat(console): hide dimension-filter chips on session pages"
```

---

## Phase 6 — End-to-end verification

### Task 13: Full click-through + cross-check

**Files:** none.

- [ ] **Step 1: Clean build**

Run: `just quality all && just test all`
Expected: all green, no warnings.

- [ ] **Step 2: End-to-end browser click-through**

Start both servers fresh. Against a DB with at least 5 real sessions (seed one via `just dev server` + actual traffic, or point at an existing duckdb file), walk through:

1. Sidebar → Agent Sessions. Rows render. Time preset works. Load more pulls page 2.
2. Apply an Agent kind filter — list narrows. Clear it — list restores.
3. Click into a session with multiple turns, some of which have long final answers. Transcript renders. Collapse/expand toggles cleanly.
4. Click "View turn detail →" on an expanded turn. Slide-over opens. Close it — transcript state is preserved (still expanded).
5. Browser back → returns to list, filter state intact.
6. Navigate to Agent Turns from the sidebar — dimension filter chips reappear in the toolbar.
7. Check an incomplete / errored session: assistant slot renders red-bordered "Turn ended without a final answer".
8. Check a session whose `source_id` is empty string: URL-encodes as `.../agent-sessions//<session_id>` — confirm the backend still resolves it. If it doesn't, decide on a sentinel and document it (separate follow-up task).

- [ ] **Step 3: Commit any final tweaks discovered during click-through**

If the walk-through revealed fixes (style regressions, broken routing, etc.), land them in focused commits with descriptive subjects.

- [ ] **Step 4: Update the spec's "Out of scope" list if needed**

If a known follow-up surfaced (e.g. empty-`source_id` sentinel, Source filter population), add it to `docs/superpowers/specs/2026-04-23-agent-sessions-ui-design.md`'s "Out of scope" section and commit with `docs: ...`.

---

## Self-review notes

**Spec coverage check:**
- Routes & sidebar → Tasks 9, 11, 12.
- List page (inbox rows, cursor infinite query, filters) → Tasks 7, 8, 9.
- Detail page (transcript, expand/collapse, view-turn-detail link) → Tasks 10, 11.
- Toolbar behavior on session pages → Task 12.
- Backend cursor + full-text extraction → Tasks 1–5.
- Error/empty/loading states → Tasks 8, 11 (inline in code).
- Out-of-scope items (search in session, export, LLM-calls↔session cross-link) → not in plan, as specified.

**Placeholder scan:** None. Each code step shows full code. Two deliberate "confirm via grep" steps (formatRelativeTime, AgentBadge) exist so the engineer adapts to whatever helpers already live in this codebase; they include fallback stub code if missing.

**Type consistency:** `SessionTurnItem` fields used in `TurnBlock` (`user_input`, `final_answer`, `total_input_tokens`, `total_output_tokens`, `call_count`, `duration_ms`, `status`, `start_time`, `turn_id`) all match Task 1's Rust struct and Task 6's TS interface. `SessionsPage.next_cursor` and `SessionTurnsPage.next_cursor` are `string | null` everywhere.
