# Turn Design

## Overview

A **Turn** in this document always means an **agent turn** — the data type is `AgentTurn`, produced only for traffic matched by an `AgentProfile` (currently `claude-cli`, `codex-cli`). Non-agent LLM traffic still lands in `LlmCall` and `LlmMetric` but never in an `AgentTurn`.

A Turn is one user interaction cycle: user submits a question → agent executes a series of LLM API calls (with tool use) → agent produces a final answer. A single Turn contains 1–N `LlmCall` records. A user session contains 1–N Turns.

## Implementation Status

This design is implemented by the `ts-turn` crate (see `server/ts-turn/`).

- Header-explicit-only policy: calls without a matching `AgentProfile` do not
  participate in turn grouping. Extending to a new agent = adding a new
  `AgentProfile` impl in `server/ts-llm/src/agents/`.
- Currently supported clients: `claude-cli` (Anthropic), `codex_cli_rs` /
  `codex-tui` (OpenAI Responses).
- **Assembly model:** buffer-and-finalize. Each `(source_id, session_id)` owns
  a `SessionBuffer` keyed by `request_time`. When a main-agent terminal call
  arrives, a small grace window (`grace_ms`, default 1000) starts; on grace
  expiry the buffer is partitioned at each terminal and one `AgentTurn` is
  emitted per partition. This tolerates arbitrary intra-shard arrival order
  (fan-in jitter, multi-connection sessions, parallel sub-agents).
- Turn boundaries: profile-defined terminal predicate (`is_turn_terminal` /
  definitive `finish_reason`) OR idle timeout (default 600 s, packet-time
  driven). Partitions without `is_user_turn_start = Some(true)` are dropped
  (counted as `TurnDiscardedNoUserStart`).

## Hierarchy

```
Session (user's coding session, may last hours)
  └── Turn 1 (user asks "fix the bug")
  │     ├── LlmCall 1  (tool_use → read file)
  │     ├── LlmCall 2  (tool_use → edit file)
  │     └── LlmCall 3  (complete → final answer)
  └── Turn 2 (user asks "add tests")
        ├── LlmCall 4  (tool_use → read file)
        └── LlmCall 5  (complete → final answer)
```

## Empirical Findings

Based on analysis of real traffic captures from Claude Code (Anthropic) and Codex (OpenAI):

### Anthropic (Claude Code) — capture2.pcap, capture4.pcap

**Protocol characteristics:**
- Stateless API (`POST /v1/messages`), client sends full conversation history each request
- No protocol-level turn/conversation ID — the API has no chaining mechanism
- Responses carry `id` (e.g., `resp_xxx`) but next request does not reference it
- Same TCP connection may be reused across turns (capture4: 2 turns on 1 connection)
- Same turn may span multiple TCP connections (capture2: 1 turn across 2 connections)

**Turn boundary signal:**
- `stop_reason` in the `message_delta` SSE event:
  - `tool_use` → agent will continue (turn in progress)
  - `end_turn` → agent is done (turn complete)
  - `max_tokens` → output truncated (turn may be incomplete)

**Client-specific headers (Claude Code):**
- `X-Claude-Code-Session-Id: <uuid>` — same across all calls in a session, used to correlate calls across connections

**Turn association strategy:**
1. Group by `X-Claude-Code-Session-Id` into a `(source_id, session_id)` buffer.
2. A request whose body's last user message is a `text` block flags
   `is_user_turn_start = Some(true)` (continuation `tool_result` blocks flag
   `Some(false)`); the user-start flag is what makes a partition emittable.
3. A response with `stop_reason = end_turn` (`FinishReason::Complete` /
   `Length`) is the main-agent terminal that starts the buffer's grace clock.
4. Without the session header → calls bucket as `(source_id, "")` — they'll
   still group by source, but cross-connection same-session calls won't merge.

### OpenAI Responses API (Codex) — capture3.pcap, capture5.pcap

**Protocol characteristics:**
- `POST /v1/responses` with SSE streaming
- Each request uses an independent TCP connection (no connection reuse)
- Protocol supports `previous_response_id` for chaining, but Codex sets it to `null`
- Protocol supports Conversations API (`conversation_id`), but Codex doesn't use it
- All responses have `status: "completed"` regardless of whether the agent continues

**Turn boundary signal:**
- NOT `status` (always "completed") — instead, look at the output items:
  - `response.output_item.done` with `type: "function_call"` → agent will call a tool and continue
  - `response.output_item.done` with `type: "message"` → agent produced a text response (turn may be ending)
- A single response can contain multiple output items (e.g., several function_calls + one message)

**Client-specific headers (Codex):**
- `X-Codex-Turn-Metadata: {"session_id":"...", "turn_id":"...", ...}` — contains explicit turn_id
- `X-Client-Request-Id: <uuid>` — matches `session_id` in the body
- Body field `session_id` — same as X-Client-Request-Id
- Body field `turn_id` — unique per turn

**Turn association strategy:**
1. Extract `session_id` (header/body) and `turn_id` (header) and bucket by
   `(source_id, session_id)`. `turn_id` is recorded as `turn_id_hint` but
   does NOT drive turn boundaries on its own — Codex's `status` is always
   `completed` and `finish_reason = Complete` only means "API call succeeded."
2. The main-agent terminal predicate is `profile.is_turn_terminal`, which
   inspects `response.output` for a terminal `message` item with no pending
   `function_call`. Pending function calls keep the buffer open.

### OpenAI Chat Completions API

Not yet observed in captures. Expected behavior based on API docs:
- `finish_reason: "tool_calls"` → agent continues
- `finish_reason: "stop"` → agent done
- Connection behavior varies by client

## Wire-API-Specific Extraction

Each wire API's extractor is responsible for:
1. Extracting `session_id` and `turn_id` from headers/body (if available)
2. Normalizing `finish_reason` to indicate whether the turn continues or ends

| Wire API | session_id source | turn_id_hint source | Main-agent terminal predicate |
|----------|------------------|---------------------|-------------------------------|
| `anthropic` | `X-Claude-Code-Session-Id` header | None (UUIDv7 minted at finalize) | `finish_reason ∈ {Complete, Length}` |
| `openai-responses` | `session_id` in body/header | `turn_id` in `X-Codex-Turn-Metadata` | `is_turn_terminal` (terminal `message` item, no pending function calls) |
| `openai-chat` | `Authorization` token prefix | None (UUIDv7 minted at finalize) | `finish_reason ∈ {Complete, Length}` |

## FinishReason Normalization

The `FinishReason` enum serves as a unified turn-continuation signal:

| FinishReason | Meaning | Anthropic source | OpenAI Chat source | OpenAI Responses source |
|-------------|---------|-----------------|-------------------|------------------------|
| `ToolUse` | Agent continues | `stop_reason: "tool_use"` | `finish_reason: "tool_calls"` | output contains only `function_call` items |
| `Complete` | Turn ends | `stop_reason: "end_turn"` | `finish_reason: "stop"` | output contains `message` item |
| `Length` | Max tokens hit | `stop_reason: "max_tokens"` | `finish_reason: "length"` | `status: "incomplete"` |
| `Error` | Generation error | (HTTP error) | (HTTP error) | `status: "failed"` |
| `Cancelled` | User cancelled | (connection close) | `finish_reason: "content_filter"` | `status: "cancelled"` |

## Assembly Model: Buffer-and-Finalize

### Motivation

The turn-stage shard receives `LlmCall`s from multiple llm-stage workers via
multi-producer `mpsc` — merge order is non-deterministic. Same-session calls
can ride on different TCP connections (multi-window CLIs, sub-agent
parallelism, HTTP keep-alive churn), so different llm-stage workers process
them concurrently with different latencies. Sub-agent calls dispatched in
parallel finish out of start-order. A naive "assume calls arrive in
`request_time` order" tracker fails with: late `is_user_start` splits a turn
into two; late `ToolUse` reverts a Complete-state turn back to open;
post-finalize stragglers create phantom turns; close decisions depending on
user-start being first to arrive.

### Design properties

**Goals**
- Correct turn assembly under arbitrary intra-shard arrival order.
- No per-call latency cost beyond a small grace period after main-agent terminal.
- Profile-agnostic mechanism: new profiles only need to declare their two
  semantic predicates (`is_main_terminal`, `is_user_turn_start`).
- All ingestion paths share the same buffer/finalize machinery — no
  parallel state machines.

**Non-goals**
- Cross-shard ordering. Shards are isolated by `hash(source, session)`; we
  only fix intra-shard order.
- Wall-clock-driven timeouts. All timing remains driven by `virtual_now_us`
  (packet-time / heartbeat) so pcap replay still works.
- Bounding worst-case finalize latency for turns that never see a terminal
  signal — those fall through to `idle_timeout_us` (sweep).

### One-line intuition

> Buffer all calls per `(source, session)`. When a main-agent terminal call
> appears, start a small per-session grace timer. On grace expiry, sort the
> buffer by `request_time` and emit one or more turns by partitioning at
> each terminal call.

The grace covers fan-in / processing jitter only — by causal logic, the
client cannot have issued a turn-terminal call until all in-flight
sub-calls for that turn are physically on the wire.

### Data structures

```rust
/// One pending call inside a SessionBuffer. arrived_at_us lets
/// multi-terminal flushes evaluate each terminal's grace against
/// when that terminal landed (not when the first one did).
struct BufferedCall {
    ic: AgentCall,
    arrived_at_us: i64,
    is_terminal: bool,  // cached is_main_terminal(profile, call)
}

/// Per (source_id, session_id) buffer.
struct SessionBuffer {
    /// Calls awaiting finalize, keyed and ordered by request_time.
    /// Vec handles request_time ties (rare but i64 µs is not collision-free).
    pending: BTreeMap<i64, Vec<BufferedCall>>,

    /// arrived_at_us of the earliest pending terminal call. Grace window
    /// expires at grace_started_at_us + grace_us. None ⇒ no terminal pending.
    grace_started_at_us: Option<i64>,

    /// High-water mark: largest request_time already emitted as part
    /// of a finalized turn. New arrivals below this are orphans.
    last_finalized_request_time: Option<i64>,
}

pub struct TurnTracker {
    registry: Arc<AgentProfileRegistry>,
    config: TrackerConfig,
    virtual_now_us: i64,
    last_sweep_us: i64,
    metrics: MetricsWorker,
    buffers: HashMap<(String, String), SessionBuffer>,
}
```

`TrackerConfig` exposes `idle_timeout_us`, `sweep_interval_us`, and
`grace_us`. `TurnEvent` collapses to a single `Completed` variant — there
is no in-progress turn state, so `Started` / `CallAdded` are unnecessary.

### ingest(AgentCall)

```
1. virtual_now_us = max(virtual_now_us, call.complete_time
                                        .or(response_time)
                                        .unwrap_or(request_time))
2. profile = registry.find_by_name(call.agent.agent_kind)
   if None: return flush_ready_buffers()
3. if profile.is_auxiliary(call): return flush_ready_buffers()    # aux never enters buffer
4. buf = buffers.entry((source_id, session_id)).or_default()
5. Late-arrival guard: if request_time < buf.last_finalized_request_time
                       → drop (TurnCallsDroppedLate++) and flush
6. is_terminal = is_main_terminal(profile, call)
7. buf.pending[request_time].push(BufferedCall {
       ic, arrived_at_us = virtual_now_us, is_terminal
   })
8. if is_terminal && buf.grace_started_at_us.is_none():
       buf.grace_started_at_us = Some(virtual_now_us)
9. return flush_ready_buffers()
```

### flush_ready_buffers

```
for (key, buf) in buffers.iter_mut():
    if buf.grace_started_at_us is Some(started)
       AND virtual_now_us ≥ started + grace_us:
           events.extend(finalize_session(profile_for(key), buf,
                                          virtual_now_us, grace_us))
return events
```

`profile_for(key)` re-resolves the profile from any pending call in the
buffer (`agent_kind` lives on every `AgentCall.agent`).

### finalize_session (grace expired)

Emit one turn per pending main-agent terminal, in arrival order, but stop
early if the *next* pending terminal hasn't yet had its own grace window
elapse. `buf.grace_started_at_us` is rewritten on each iteration to point at
the next-pending terminal's `arrived_at_us` (or `None` if none remain).

```
loop:
    sorted = all pending calls in request_time order
    if sorted.is_empty():
        buf.grace_started_at_us = None; break

    terminal_idx = sorted.iter().position(|bc| bc.is_terminal)
    match terminal_idx:
        None:
            buf.grace_started_at_us = None; break
        Some(idx):
            front_arrived = sorted[idx].arrived_at_us
            if virtual_now_us < front_arrived + grace_us:
                buf.grace_started_at_us = Some(front_arrived)   # reseat
                break

            terminal_ts = sorted[idx].request_time
            # Partition: everything with request_time ≤ terminal_ts → this turn.
            turn_calls, rest = split at first call with request_time > terminal_ts

            # Discard rule: need at least one is_user_turn_start = Some(true)
            if turn_calls.has_user_start():
                events.push(TurnEvent::Completed(build_turn(profile, turn_calls)))
                Counter TurnFinalizedByGrace++
            else:
                Counter TurnDiscardedNoUserStart++

            buf.last_finalized_request_time = Some(terminal_ts)
            buf.pending = rest
            # loop: check next terminal's own grace
```

Two consequences:

- A late terminal that arrives *after* an earlier terminal's grace fires
  triggers its own grace window on its own arrival timestamp. No turn is
  finalized "early."
- A non-terminal sub-call between two terminals (by `request_time`) joins
  the *earlier* turn — the client cannot have started turn N+1 until turn
  N's terminal call was issued.

### is_main_terminal

```rust
fn is_main_terminal(profile: &dyn AgentProfile, call: &LlmCall) -> bool {
    if profile.subagent(call).is_some() { return false; }   // sub-agent never terminates parent
    if profile.is_turn_terminal(call) { return true; }      // explicit (Codex: response.output)
    matches!(call.finish_reason, Some(FinishReason::Complete | FinishReason::Length))
}
```

`Error` / `Cancelled` are deliberately excluded. The client may retry within
the same logical turn — the call stays buffered; either a retry's real
Complete joins the partition, or the idle sweep emits Incomplete.

### build_turn(profile, calls)

Pure function over a sorted, complete call list — no order-dependent merge.

- `start_time_us = calls[0].request_time`
- `end_time_us = calls.last().last_activity()` (max of complete/response/request)
- `call_count`, `call_ids`, token sums: folds over `calls`
- `models_used`, `subagents_used`: ordered-unique fold
- `user_input_preview` / `user_call_id`: first call where
  `profile.extract_user_input(call)` is some (prefer `is_user_turn_start`-tagged)
- `final_answer_preview` / `final_call_id`: last MAIN-AGENT call (sub-agent
  text never leaks here) via `profile.extract_assistant_text`
- `final_finish_reason`: last main-agent call's `finish_reason`
- `status`: derived from the final main-agent finish_reason (Complete →
  Complete, Length → Length, etc.; Incomplete on the idle path)

### advance_time / sweep / flush_all

```
advance_time(ts):
    virtual_now_us = max(virtual_now_us, ts)
    flush_ready_buffers() + sweep()

sweep() (idle fallback, same sweep_interval_us throttle):
    for each buffer where pending non-empty AND no main-agent terminal
       AND newest pending older than idle_timeout_us:
          drain all
          if any is_user_turn_start → emit Incomplete turn (TurnFinalizedByIdle++)
          else → drop (TurnDiscardedNoUserStart++)
          update last_finalized_request_time to largest drained request_time

flush_all() (EOF):
    per-session: partition by terminals as in finalize_session (discard rule applies).
    Any remainder without a terminal → sweep-style: Incomplete if user-start, else drop.
```

Virtual-time-only: all timeouts driven by `virtual_now_us` (packet-time /
heartbeat), never wall clock. pcap replay works end-to-end without
special-casing.

### Buffer lifecycle / GC

After successful finalize, if `pending.is_empty()` AND
`last_finalized_request_time + 2 · idle_timeout < virtual_now`, the
`SessionBuffer` entry is dropped. (Loses orphan detection for that session,
but that's well past plausible reorder.)

`turn_id` is generated as a UUIDv7 at finalize time (monotonic by emission).

## Configuration

| Field | Default | Notes |
|---|---|---|
| `grace_us` | 1_000_000 (1 s) | Covers fan-in / processing jitter. Tunable; counters below tell us if we need more. |
| `idle_timeout_us` | 600_000_000 (600 s) | Fallback for turns that never see a terminal signal. |
| `sweep_interval_us` | 10_000_000 (10 s) | How often the idle sweep runs. |

`tokenscope.toml`:

```toml
[turn]
grace_ms = 1000
idle_timeout_s = 600
sweep_interval_s = 10
```

## Failure Modes (operator-visible counters)

| Counter | Meaning | What to look at if rising |
|---|---|---|
| `worker::turn::calls_late` (`TurnCallsDroppedLate`) | Late call dropped at entry guard | Cross-shard hashing bug, severe fan-in jitter, replay-with-time-skew |
| `worker::turn::no_user_start` (`TurnDiscardedNoUserStart`) | Partition discarded for lack of user-start call | Lost capture window at session boundary; orphan sub-agent traffic; profile mis-classifying user-start |
| `worker::turn::fin_idle` (`TurnFinalizedByIdle`) | Turn closed by idle timeout, not by terminal call | Truncated capture, client crash, missing terminal signal in profile |
| `worker::turn::fin_grace` (`TurnFinalizedByGrace`) | Turn closed normally via grace expiry | Healthy steady-state path; ratio vs `fin_idle` is the health signal |

## Edge Cases

| # | Case | Handling |
|---|---|---|
| 1 | Cross-connection turns (Anthropic) | Group by `(source_id, session_id)`, not by TCP 4-tuple |
| 2 | No finish_reason (truncated capture) | Call stays in buffer; idle sweep emits `Incomplete` (or discards) |
| 3 | HTTP errors mid-turn | `Error`/`Cancelled` excluded from `is_main_terminal`; retry joins the same partition, else idle → Incomplete |
| 4 | Multiple turns on same connection (Anthropic) | capture4 shows 2 turns on 1 connection — `end_turn` marks boundary, next request starts next turn |
| 5 | No client headers (generic clients) | Fall back to `(source_id, "")` + finish_reason. Cross-connection same-session won't merge. |
| 6 | Parallel calls within a turn | All share the same buffer; sort-then-partition is order-independent |
| 7 | Sub-agent Complete before main-agent terminal | Sub-agent excluded from `is_main_terminal`; grace not started; no spurious finalize |
| 8 | Sub-agent assistant text leaking to parent | `build_turn` picks final-call from main-agent only |
| 9 | Single-call Error retry | `Error` excluded from `is_main_terminal`; buffer retained until real Complete or idle sweep |
| 10 | Two terminals pending at flush time | `finalize_session` loops; each terminal's grace checked against own `arrived_at_us` |
| 11 | Late call after finalize | Entry-guard drop via `last_finalized_request_time`, counted via `TurnCallsDroppedLate` |
| 12 | No terminal ever | Idle sweep emits Incomplete (or discards if no user-start) |
| 13 | pcap replay (no heartbeats) | Last buffered batch waits for the next call to advance `virtual_now`, or for EOF `flush_all` |
| 14 | Buffer memory growth (long-lived idle sessions) | GC after `2 · idle_timeout` past last finalize |
| 15 | Empty session_id from profile | `(source, "")` key; not special-cased |
| 16 | Codex new turn_id arrives mid-grace of old turn_id | Same buffer; old's terminal triggers grace; finalize old at grace; new turn_id calls remain for their own terminal |
| 17 | Continuation/sub-agent calls without user-start in their partition | Discarded at finalize/sweep/flush; `TurnDiscardedNoUserStart` |

## Data Model

`LlmCall` is wire-API-shaped raw data. Agent attribution lives in
`AgentIdentity`, attached by `ts-llm/src/stage.rs` before the call enters the
turn shard. An `AgentCall` is `{ call: Arc<LlmCall>, agent: AgentIdentity }`.

`AgentIdentity` fields:

- `agent_kind: &'static str` — selects the `AgentProfile` impl and is the
  value persisted to `AgentTurn.agent_kind` (e.g. `"claude-cli"`).
- `session_id: String` — extracted by the profile.
- `turn_id_hint: Option<String>` — set by Codex; informational only.

`LlmCall.finish_reason: Option<FinishReason>` carries the normalized
terminal signal (see table above).

The aggregated `AgentTurn` (see `ts-turn/src/model.rs`) is built at finalize
time from the sorted partition:

- `turn_id: String` (UUIDv7), `session_id`, `source_id`, `agent_kind`,
  `wire_api`
- `start_time_us`, `end_time_us`, `duration_ms`, `call_count`, `call_ids`
- `models_used`, `subagents_used`
- `total_input_tokens`, `total_output_tokens`,
  `total_cache_read_input_tokens`, `total_cache_creation_input_tokens`
- `status: TurnStatus` — Complete / Length / Cancelled / Failed / Incomplete
- `final_finish_reason`, `user_input_preview`, `user_call_id`,
  `final_answer_preview`, `final_call_id`

## Design Rationale

- **Per-terminal grace.** Each `BufferedCall` records its own `arrived_at_us`. When two terminals are pending, each gets its own grace window measured from its own arrival — a second terminal arriving later doesn't get rushed because the first one already finalized.
- **Order-independent turn fields.** All fields (`final_call_id`, `user_call_id`, `end_time_us`, etc.) derive from the sorted partition, not from arrival order. A late `is_user_turn_start` call lands in the right turn. Eliminates the order-dependent merge overwrite bug entirely.
- **Sub-agent isolation.** Sub-agent calls never trigger main-agent grace, and their assistant text never appears in the parent's `final_answer_preview`.
- **No new profile methods.** Uses only existing predicates (`subagent`, `is_turn_terminal`, `is_user_turn_start`, `extract_user_input`, `extract_assistant_text`, `is_auxiliary`). New profiles slot in without changing the trait.
- **No-user-start partitions discarded.** A finalized partition with zero `is_user_turn_start = Some(true)` calls is dropped, not emitted. Covers stray continuations, sub-agent leftovers from missing parents, and late stragglers that get past the orphan check but represent no real turn.
- **Cross-source independence.** `(source_id, session_id)` is the buffer key; same `session_id` under different `source_id` is treated as independent sessions by design. Clients don't share sessions across sources.
