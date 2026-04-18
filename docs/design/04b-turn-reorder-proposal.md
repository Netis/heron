# Turn Reorder Buffer — Design

**Status:** ACCEPTED and IMPLEMENTED in `ts-turn/src/tracker.rs` (see
`tests/reorder.rs` for the failure scenarios this resolves).
**Scope:** `server/ts-turn/src/tracker.rs`, minor `stage.rs` API change.
**Companion to:** `04-turn.md` (which now points here for the assembly model).

---

## 1. Problem

Today's `TurnTracker` assumes that calls within a `(stream_id, session_id)`
arrive at `ingest` in `request_time` order. The fan-in path violates this:

- The turn-stage shard receives `LlmCall`s from multiple llm-stage workers via
  multi-producer `mpsc`. Merge order is non-deterministic.
- Same-session calls can ride on different TCP connections (multi-window
  CLIs, sub-agent parallelism, HTTP keep-alive churn) → different llm-stage
  workers process them concurrently with different latencies.
- Sub-agent calls dispatched in parallel finish out of start-order.

Concrete failures (catalogued in conversation): late `is_user_start` splits a
turn into two; late `ToolUse` reverts a Complete-state turn back to open;
post-finalize stragglers create phantom turns; `confident_single` close depends
on user-start being first to arrive; etc.

## 2. Goals / Non-goals

**Goals**
- Correct turn assembly under arbitrary intra-shard arrival order.
- No per-call latency cost beyond a small grace period after main-agent terminal.
- Profile-agnostic mechanism: new profiles only need to declare their two
  semantic predicates (`is_main_terminal`, `is_user_turn_start`).
- All ingestion paths share the same buffer/finalize machinery — no
  parallel state machines.

**Non-goals**
- Cross-shard ordering. Shards are isolated by `hash(stream, session)`; we
  only fix intra-shard order.
- Wall-clock-driven timeouts. All timing remains driven by `virtual_now_us`
  (packet-time / heartbeat) so pcap replay still works.
- Bounding worst-case finalize latency for turns that never see a terminal
  signal — those continue to fall through to `idle_timeout_us` (sweep).

## 3. Approach (one line)

> Buffer all calls per `(stream, session)`. When a main-agent terminal call
> appears, start a small per-session grace timer. On grace expiry, sort the
> buffer by `request_time` and emit one or more turns by partitioning at
> each terminal call.

The grace covers fan-in / processing jitter only — by causal logic, the
client cannot have issued a turn-terminal call until all in-flight
sub-calls for that turn are physically on the wire.

## 4. Public API changes

`TurnTracker::ingest` changes from borrowed args to owned `IdentifiedCall`
(needed because the call may be retained in the buffer):

```rust
// before
pub fn ingest(&mut self, call: &LlmCall, identity: &CallIdentity) -> Vec<TurnEvent>;

// after
pub fn ingest(&mut self, identified: IdentifiedCall) -> Vec<TurnEvent>;
```

`IdentifiedCall.call` is already `Arc<LlmCall>` so this is cheap. Stage.rs
caller change is a single line. All other public signatures (`advance_time`,
`sweep`, `flush_all`, `active_count`, `virtual_now_us`) are unchanged.

`TrackerConfig` gains one field:

```rust
pub struct TrackerConfig {
    pub idle_timeout_us: i64,
    pub sweep_interval_us: i64,
    pub grace_us: i64,            // NEW; default 1_000_000 (1 s)
}
```

## 5. Data structures

```rust
/// One pending call inside a SessionBuffer. We track the local arrival
/// instant so that, on multi-terminal flushes, each terminal's grace can
/// be evaluated against when *that* terminal landed (not when the first
/// one did). See §6.3.
#[derive(Debug)]
struct BufferedCall {
    ic: IdentifiedCall,
    /// virtual_now_us at the moment ingest stored this call.
    arrived_at_us: i64,
    /// Cached `is_main_terminal(profile, &ic.call)` so finalize doesn't
    /// re-resolve profile per element.
    is_terminal: bool,
}

/// Per (stream_id, session_id) buffer. Lives inside TurnTracker.
#[derive(Default, Debug)]
struct SessionBuffer {
    /// Calls awaiting finalize, keyed and ordered by request_time.
    /// Vec handles request_time ties (rare but i64 µs is not collision-free).
    pending: BTreeMap<i64, Vec<BufferedCall>>,

    /// `arrived_at_us` of the earliest pending terminal call. The grace
    /// window for that terminal expires at `grace_started_at_us + grace_us`.
    /// `None` ⇒ no terminal currently pending. Set by ingest when a terminal
    /// is buffered; recomputed by finalize_session after each emitted turn
    /// (next pending terminal's arrival, or `None` if none remain).
    grace_started_at_us: Option<i64>,

    /// Largest request_time we've already emitted as part of a finalized turn.
    /// New arrivals with request_time < this are orphans (drop + count).
    last_finalized_request_time: Option<i64>,
}

pub struct TurnTracker {
    // existing fields
    registry: Arc<ProfileRegistry>,
    config: TrackerConfig,
    virtual_now_us: i64,
    last_sweep_us: i64,
    metrics: MetricsWorker,

    // new — replaces the old `active: HashMap<TurnKey, ActiveTurn>`
    buffers: HashMap<(String, String), SessionBuffer>,
}
```

New metrics (counters):

| Name | Increment when |
|---|---|
| `TurnReorderOrphan` | Late call dropped at buffer entry |
| `TurnFinalizedByGrace` | Turn finalized via grace expiry |
| `TurnFinalizedByIdle` | Turn finalized via idle-timeout sweep |
| `TurnDiscardedNoUserStart` | Partition dropped at finalize because no call carried `is_user_turn_start = Some(true)` (see §6.3) |
| `TurnBufferedCallsCurrent` | Gauge of total buffered calls (optional) |

## 6. Core algorithms

### 6.1 `ingest(IdentifiedCall) -> Vec<TurnEvent>`

```
1. Push virtual_now_us = max(virtual_now_us, ic.call.complete_time
                                              .or(response_time)
                                              .unwrap_or(request_time))
2. Counter TurnCallsIngested++
3. profile = registry.find_by_name(ic.identity.profile_name)
   if None: return flush_ready_buffers()       # still drain
4. if profile.is_auxiliary(&ic.call):
       Counter TurnCallsAuxiliary++
       return flush_ready_buffers()            # aux never enters buffer
5. key = (ic.call.stream_id, ic.identity.session_id)
   buf = buffers.entry(key).or_default()
6. # Orphan check (entry guard)
   if let Some(hw) = buf.last_finalized_request_time:
       if ic.call.request_time < hw:
           Counter TurnReorderOrphan++
           return flush_ready_buffers()
7. is_terminal = is_main_terminal(profile, &ic.call)   # see §6.5
8. buf.pending.entry(ic.call.request_time).or_default()
       .push(BufferedCall { ic, arrived_at_us: virtual_now_us, is_terminal })
9. if is_terminal && buf.grace_started_at_us.is_none():
       # First pending terminal — start its grace clock. If a terminal
       # was already pending, leave its arrival timestamp in place;
       # finalize_session will pick the next one's arrival when it pops.
       buf.grace_started_at_us = Some(virtual_now_us)
10. return flush_ready_buffers()
```

### 6.2 `flush_ready_buffers() -> Vec<TurnEvent>`

```
events = []
for (key, buf) in buffers.iter_mut():
    if let Some(started) = buf.grace_started_at_us:
        if virtual_now_us < started + config.grace_us:
            continue                               # still in grace
        # Grace expired for the front terminal — let finalize_session
        # emit one or more turns and update buf.grace_started_at_us
        # itself (it knows when each next terminal arrived).
        events.extend(finalize_session(profile_for(key), buf,
                                       virtual_now_us, config.grace_us))
return events
```

`profile_for(key)` re-resolves profile from any pending call in the buffer
(profile_name lives on every `IdentifiedCall`'s `identity`, so we read one).
We don't store `profile` on the buffer itself — it's a `&dyn` borrowed from
the registry.

### 6.3 `finalize_session(profile, buf, now_us, grace_us) -> Vec<TurnEvent>`

Caller has already verified that `buf.grace_started_at_us`'s grace has
expired. We emit one turn per pending main-agent terminal, in arrival
order, but stop early if the *next* pending terminal hasn't yet had its
own grace window elapse. The buffer's `grace_started_at_us` is rewritten
on every loop iteration to point at the next-pending terminal's
`arrived_at_us` (or `None` if no terminal remains).

```
events = []
loop:
    # Gather all pending calls in request_time order.
    sorted: Vec<&BufferedCall> = buf.pending.values().flatten().collect()
    if sorted.is_empty():
        buf.grace_started_at_us = None
        break

    # Find earliest main-agent terminal (sub-agent terminals don't count).
    terminal_idx = sorted.iter().position(|bc| bc.is_terminal)
    match terminal_idx:
        None:
            # No terminal pending → no grace clock to maintain.
            buf.grace_started_at_us = None
            break
        Some(idx):
            front_arrived = sorted[idx].arrived_at_us
            if now_us < front_arrived + grace_us:
                # This terminal still inside its own grace window.
                # Update the timer to reflect *its* arrival (not the
                # earlier one we just consumed) and stop.
                buf.grace_started_at_us = Some(front_arrived)
                break

            terminal_ts = sorted[idx].ic.call.request_time
            # Partition: everything with request_time ≤ terminal_ts → this turn.
            # (≤, not <. The terminal call itself belongs to the turn.)
            (turn_calls, rest) = split sorted at first index where
                                 ic.call.request_time > terminal_ts
            # turn_calls covers indices 0..=idx (BTreeMap order invariant).

            # Discard rule: a real turn must contain at least one call
            # the profile flags as `is_user_turn_start = Some(true)`. A
            # partition without one is a stray (lost user-start, sub-agent
            # leftover from a missing parent, or a continuation orphaned
            # by a finalized predecessor). We consume the calls (advance
            # high-water) but emit nothing.
            if turn_calls.iter().any(|bc| matches!(
                profile.is_user_turn_start(&bc.ic.call), Some(true)
            )) {
                events.push(TurnEvent::Completed(
                    build_turn(profile, turn_calls.iter().map(|bc| &bc.ic))
                ))
                Counter TurnFinalizedByGrace++
                Counter TurnsCompleted++
            } else {
                Counter TurnDiscardedNoUserStart++
            }

            buf.last_finalized_request_time = Some(terminal_ts)
            buf.pending = rebuild_btreemap_from(rest)
            # Loop: next iteration will recompute `terminal_idx` against
            # whatever remains, and check that next terminal's grace.
return events
```

Two consequences worth calling out:

- A late terminal that arrives *after* an earlier terminal's grace fires
  will simply trigger its own grace window on its own arrival timestamp.
  No turn is finalized "early".
- If a non-terminal sub-call lands between two terminals (request_time
  between them), it goes with the *earlier* turn, since partition is
  `request_time ≤ terminal_ts`. This matches client causal semantics:
  the client cannot have started turn N+1 until turn N's terminal call
  was issued.

`build_turn(profile, calls)`: replaces today's `ActiveTurn::merge` + `finalize`.
Pure function over a sorted, complete call list:

- `start_time_us = calls[0].request_time`
- `end_time_us = calls.last().last_activity()`  (max of complete/response/request)
- `call_count`, `call_ids`, token sums: straightforward folds over `calls`
- `models_used`, `subagents_used`: ordered-unique fold
- **`user_input_preview` / `user_call_id`**: pick the first call where
  `profile.extract_user_input(call).is_some()`. If none, leave `None`.
  (Prefer `is_user_turn_start`-tagged call if available.)
- **`final_answer_preview` / `final_call_id`**: the last MAIN-AGENT call
  (i.e., excluding sub-agent calls). Use `profile.extract_assistant_text`.
- `final_finish_reason`: from the last main-agent call's `finish_reason`.
- `status`: derived from final main-agent finish_reason (Complete → Complete,
  Length → Length, etc.; nothing → Incomplete for idle path).

This eliminates the order-dependent `merge` overwrite bug entirely — turn
fields are derived from the sorted set.

### 6.4 What happens to `active` and the per-call events?

In the current design, `active` holds the in-progress turn and `ingest`
emits `TurnEvent::Started` / `TurnEvent::CallAdded` as calls arrive.
Under proposal B there is no "in-progress turn" — calls live in a raw
buffer until a terminal lands and grace fires. So:

- **`active` is removed entirely.** Tracker state becomes
  `buffers: HashMap<(stream, session), SessionBuffer>`.
  `active_count()` returns `buffers.values().map(|b| b.pending.values().map(|v| v.len()).sum()).sum()` (or simply `buffers.len()` for the per-session liveness signal — pick whichever the existing call-sites need).
- **`TurnEvent::Started` and `TurnEvent::CallAdded` are removed.** Only
  `TurnEvent::Completed` remains. The variants and any code that emits
  or matches them are deleted; the enum collapses to a single payload.
  Confirmed in `stage.rs`: only `Completed` is consumed today, so no
  downstream wiring change is required beyond the `match` cleanup.

### 6.5 `is_main_terminal(profile, call) -> bool`

Predicate centralizing "this call ends a turn". Encapsulates today's two
sources:

```
fn is_main_terminal(profile: &dyn ClientProfile, call: &LlmCall) -> bool {
    // Sub-agent calls never terminate the parent turn.
    if profile.subagent(call).is_some() { return false; }
    // Explicit-path: profile-defined predicate (Codex inspects response.output).
    if profile.is_turn_terminal(call) { return true; }
    // Implicit-path: definitive finish reasons. Error/Cancelled deliberately
    // excluded for single-call turns — client may retry within the same logical
    // turn. (See §7.3 for the single-call special case.)
    matches!(call.finish_reason, Some(FinishReason::Complete | FinishReason::Length))
}
```

For Codex, `profile.is_turn_terminal` already handles the "no pending tool
calls in response.output" check. For Anthropic, it returns `false` and we
fall through to the finish_reason check.

### 6.6 `advance_time(ts) -> Vec<TurnEvent>`

```
virtual_now_us = max(virtual_now_us, ts)
events = flush_ready_buffers()
events.extend(sweep())
return events
```

### 6.7 `sweep() -> Vec<TurnEvent>` (idle fallback)

For each `SessionBuffer` whose `pending` is non-empty AND has no main-agent
terminal AND whose newest pending call is older than `idle_timeout_us`:

```
- Drain all pending calls.
- Apply the §6.3 discard rule: if no drained call has
  is_user_turn_start = Some(true), bump TurnDiscardedNoUserStart and
  emit nothing. Otherwise build one Incomplete turn via build_turn()
  and bump TurnFinalizedByIdle.
- Update last_finalized_request_time to the largest drained request_time
  in either case (the calls have been consumed).
```

Same `sweep_interval_us` throttle as today.

### 6.8 `flush_all() -> Vec<TurnEvent>` (EOF)

Drain every `SessionBuffer` regardless of grace state. For each session:
- If buffer has a main-agent terminal, partition by terminals as in 6.3
  (subject to the discard rule).
- Then any remainder (no terminal) is treated like the §6.7 sweep
  partition: emit one Incomplete turn if it contains a user_turn_start,
  otherwise drop and bump `TurnDiscardedNoUserStart`.

## 7. Edge cases

| # | Case | Handling |
|---|---|---|
| 7.1 | Sub-agent Complete arrives before main-agent terminal | Sub-agent excluded from `is_main_terminal`; grace not started; no spurious finalize |
| 7.2 | Sub-agent assistant text leaks to parent | `build_turn` picks final-call from main-agent only; sub-agent text never assigned to `final_answer_preview` |
| 7.3 | Single-call Error retry | `Error` excluded from `is_main_terminal`; turn stays buffered; retry call (same session) joins; eventually a real Complete arrives or idle-timeout |
| 7.4 | Two terminals pending at flush time | `finalize_session` loops; each terminal's grace is checked against its own `arrived_at_us`. First terminal whose grace hasn't expired stops the loop and reseats `grace_started_at_us` to that terminal's arrival. |
| 7.5 | Late call after finalize (orphan) | Entry guard at step 6 of §6.1; counter + drop |
| 7.6 | No terminal ever | Falls through to `sweep()` idle path (§6.7) |
| 7.7 | pcap replay (no heartbeats) | Last buffered batch waits for next call to advance virtual_now or for EOF flush_all |
| 7.8 | Buffer memory growth (long-lived idle sessions) | After successful finalize, if `pending.is_empty()` AND `last_finalized_request_time + 2·idle_timeout < virtual_now`, drop the SessionBuffer entry. (Loses orphan detection for that session, but well past plausible reorder.) |
| 7.9 | Empty session_id from profile | Same `(stream, "")` key behavior as today; not new |
| 7.10 | Codex new turn_id arrives mid-grace of old turn_id | Same buffer; old's terminal triggers grace; finalize old at grace; new turn_id calls remain in buffer for their own terminal. (No "stale_keys" cleanup needed; replaced by partition logic.) |
| 7.11 | Continuation/sub-agent calls without a user_turn_start in their partition | Discarded at finalize (§6.3) / sweep (§6.7) / flush (§6.8). Counted via `TurnDiscardedNoUserStart`. Covers: lost user-start packets, sub-agent calls whose parent main-agent call was missed, late stragglers that slip past the orphan check but represent no real turn. |

## 8. Configuration

| Field | Default | Notes |
|---|---|---|
| `grace_us` | 1_000_000 (1 s) | Tunable; observability counters tell us if we need more |
| `idle_timeout_us` | 600_000_000 (600 s) | Unchanged |
| `sweep_interval_us` | 10_000_000 (10 s) | Unchanged |

Also expose to `tokenscope.toml`:

```toml
[turn]
grace_ms = 1000
idle_timeout_s = 600
sweep_interval_s = 10
```

## 9. Migration plan

Three commits, each independently reviewable and revertable:

**Commit 1 — failing reorder tests.**
Add a `tests/reorder.rs` integration suite that constructs the failure
scenarios catalogued in conversation (A–F) using the existing tracker
API. These tests should FAIL on `main`, demonstrating the bugs concretely
and giving us a red bar to drive the implementation.

**Commit 2 — buffer + finalize implementation.**
Implement §§4–7. Update `stage.rs` to pass `IdentifiedCall` by value.
Replace `active` HashMap with `buffers`. Rewrite `ActiveTurn::merge` →
`build_turn` pure function. Collapse `TurnEvent` to a single `Completed`
variant; remove `Started` / `CallAdded` and any code that emits or
matches them.

Existing unit tests need rewriting (not just patching) because the
event surface changes:
- Anything asserting on `Started` / `CallAdded` is deleted.
- Tests that previously got a `Completed` "for free" mid-stream now need
  to drive `advance_time(arrival + grace_us + 1)` (or, for a tighter
  loop, set `grace_us = 0` in the test config) before the assertion.

**Commit 3 — defaults + docs.**
Set `grace_us = 1_000_000` default. Update `04-turn.md` to point at this
proposal. Add operator notes on the new metrics counters.

## 10. Test plan

**Unit (in tracker.rs)**
- All existing tests pass with `grace_us=0` (where applicable, with explicit
  `advance_time` after each ingest).
- New: each scenario from the conversation catalogue (A–F) — both as
  "in-order baseline still passes" and "scrambled order also passes".
- Buffer cleanup: idle session's SessionBuffer is GC'd by sweep.
- Orphan: late call after finalize → dropped + counter incremented.

**Integration (tests/reorder.rs)**
- Two sub-agents finishing out-of-order.
- User-start arriving after continuation.
- Two turns finalized in a single grace expiry.
- Codex turn_id transition with reordered intra-turn calls.
- Empty buffer + heartbeat does nothing.
- pcap-replay-style: drive only via call timestamps, no heartbeats.

**Stage-level**
- `stage.rs` integration tests still pass after the `IdentifiedCall`
  by-value API change.

## 11. Resolved decisions

1. **Grace semantics on second terminal.** Per-terminal tracking adopted.
   Each `BufferedCall` records its `arrived_at_us`; `finalize_session`
   reseats `buf.grace_started_at_us` to the next pending terminal's
   arrival after every emitted turn (§6.3). Both terminals finalizing
   together only happens when both grace windows have actually expired.

2. **`Started` / `CallAdded` events.** Removed. `TurnEvent` collapses to
   a single `Completed` variant. `stage.rs` only consumes `Completed`
   today, so no downstream wiring change is required.

3. **Per-session GC threshold.** §7.8's `2·idle_timeout` accepted as the
   inline rule. Not promoting to its own config knob until we have
   evidence it needs tuning.

4. **Default `grace_us`.** `1_000_000` (1 s). Conservative starting
   point; revisit after observing `TurnFinalizedByGrace` /
   `TurnFinalizedByIdle` ratios in real deployments.

5. **Cross-stream sessions.** `(stream_id, session_id)` is the buffer
   key; same `session_id` under different `stream_id` is treated as
   independent sessions by design. (Confirmed: clients don't share
   sessions across streams.)

6. **Profile interface.** No new trait methods required. The proposal
   uses only existing predicates (`subagent`, `is_turn_terminal`,
   `is_user_turn_start`, `extract_user_input`, `extract_assistant_text`,
   `is_auxiliary`).

7. **No-user-start partitions are discarded.** A finalized partition
   that contains zero calls with `is_user_turn_start = Some(true)` is
   dropped (counted via `TurnDiscardedNoUserStart`) instead of emitted.
   Applies in finalize (§6.3), idle sweep (§6.7), and EOF flush (§6.8).
   Covers stray continuations, sub-agent leftovers from missing parents,
   and late stragglers that get past the orphan check but represent no
   real turn. (Replaces the earlier ignored Test E "should sub-agent
   merge with the next user message" question — the answer is "no
   special-casing; the discard rule handles it together with all other
   no-user-start cases".)

## 12. What this proposal explicitly does NOT do

- Does not redesign the profile trait or introduce `Lifecycle` enum.
- Does not change sharding or fan-in mechanics in `ts-llm/src/stage.rs`.
- Does not change the `LlmTurn` schema or storage format.
- Does not touch the metrics aggregation pipeline.
