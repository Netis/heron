# Turn Design

## Overview

A **Turn** is one user interaction cycle: user submits a question → agent executes a series of LLM API calls (with tool use) → agent produces a final answer. A single Turn contains 1–N `LlmCall` records. A user session contains 1–N Turns.

## Implementation Status

This design is implemented by the `ts-turn` crate (see `server/ts-turn/`).

- Header-explicit-only policy: calls without a matching `ClientProfile` do not
  participate in turn grouping. Extending to a new client = adding a new
  `ClientProfile` impl in `server/ts-turn/src/profiles/`.
- Currently supported clients: `claude-cli` (Anthropic), `codex_cli_rs` /
  `codex-tui` (OpenAI Responses).
- **Assembly model:** buffer-and-finalize. Each `(stream_id, session_id)` owns
  a `SessionBuffer` keyed by `request_time`. When a main-agent terminal call
  arrives, a small grace window (`grace_ms`, default 1000) starts; on grace
  expiry the buffer is partitioned at each terminal and one `LlmTurn` is
  emitted per partition. This tolerates arbitrary intra-shard arrival order
  (fan-in jitter, multi-connection sessions, parallel sub-agents). See
  `04b-turn-reorder-proposal.md` for the full algorithm and rationale.
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
1. Group by `X-Claude-Code-Session-Id` into a `(stream_id, session_id)` buffer.
2. A request whose body's last user message is a `text` block flags
   `is_user_turn_start = Some(true)` (continuation `tool_result` blocks flag
   `Some(false)`); the user-start flag is what makes a partition emittable.
3. A response with `stop_reason = end_turn` (`FinishReason::Complete` /
   `Length`) is the main-agent terminal that starts the buffer's grace clock.
4. Without the session header → calls bucket as `(stream_id, "")` — they'll
   still group by stream, but cross-connection same-session calls won't merge.

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
   `(stream_id, session_id)`. `turn_id` is recorded as `turn_id_hint` but
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

## Provider-Specific Extraction

Each provider's extractor is responsible for:
1. Extracting `session_id` and `turn_id` from headers/body (if available)
2. Normalizing `finish_reason` to indicate whether the turn continues or ends

| Provider | session_id source | turn_id_hint source | Main-agent terminal predicate |
|----------|------------------|---------------------|-------------------------------|
| Anthropic | `X-Claude-Code-Session-Id` header | None (UUIDv7 minted at finalize) | `finish_reason ∈ {Complete, Length}` |
| OpenAI Responses | `session_id` in body/header | `turn_id` in `X-Codex-Turn-Metadata` | `is_turn_terminal` (terminal `message` item, no pending function calls) |
| OpenAI Chat | `Authorization` token prefix | None (UUIDv7 minted at finalize) | `finish_reason ∈ {Complete, Length}` |

## FinishReason Normalization

The `FinishReason` enum serves as a unified turn-continuation signal:

| FinishReason | Meaning | Anthropic source | OpenAI Chat source | OpenAI Responses source |
|-------------|---------|-----------------|-------------------|------------------------|
| `ToolUse` | Agent continues | `stop_reason: "tool_use"` | `finish_reason: "tool_calls"` | output contains only `function_call` items |
| `Complete` | Turn ends | `stop_reason: "end_turn"` | `finish_reason: "stop"` | output contains `message` item |
| `Length` | Max tokens hit | `stop_reason: "max_tokens"` | `finish_reason: "length"` | `status: "incomplete"` |
| `Error` | Generation error | (HTTP error) | (HTTP error) | `status: "failed"` |
| `Cancelled` | User cancelled | (connection close) | `finish_reason: "content_filter"` | `status: "cancelled"` |

## Buffer-and-Finalize Assembly

Each `(stream_id, session_id)` owns a `SessionBuffer` of pending calls
ordered by `request_time`. The tracker is a passive state container driven by
`ingest`, `advance_time`, `sweep`, `flush_all` — all timing is virtual
(packet/heartbeat), not wall clock.

```
ingest(IdentifiedCall):
  - bump virtual_now to call's last activity
  - orphan guard: drop if request_time < buffer's last_finalized_request_time
  - append to pending[request_time]
  - if call is a main-agent terminal and no terminal was already pending,
    start the grace clock at virtual_now
  - flush any buffer whose grace window expired

flush (grace expired):
  - sort pending by request_time
  - for each main-agent terminal whose own grace has expired:
      - partition: take every pending call with request_time ≤ terminal_ts
      - if partition contains a user-turn-start call → emit one LlmTurn
        else → drop the partition (TurnDiscardedNoUserStart)
      - record terminal_ts as last_finalized_request_time
  - reseat grace clock to the next pending terminal's arrival, or clear

sweep (idle timeout):
  - drain any session whose newest pending call is older than idle_timeout
    AND which has no main-agent terminal — emit Incomplete (or discard).

flush_all (EOF):
  - force grace open and run finalize, then drain any non-terminal tail.
```

Key properties:

- **Order-independent:** turn fields (`final_call_id`, `user_call_id`,
  `end_time_us`, etc.) derive from the sorted partition, not from arrival
  order. A late `is_user_turn_start` call lands in the right turn.
- **Sub-agent isolation:** sub-agent calls never trigger main-agent grace,
  and their assistant text never appears in the parent's `final_answer_preview`.
- **Per-terminal grace:** when two terminals are pending, each gets its own
  grace window measured from its own `arrived_at_us`. A second terminal that
  arrives later doesn't get rushed because the first one already finalized.
- **Orphan guard:** a call older than the buffer's high-water mark is dropped
  and counted (`TurnReorderOrphan`) instead of opening a phantom turn.
- **Memory:** drained buffers idle past `2 × idle_timeout_us` are GC'd.

`turn_id` is generated as a UUIDv7 at finalize time (monotonic by emission).

## Failure Modes (operator-visible counters)

| Counter | Meaning | What to look at if rising |
|---|---|---|
| `worker::turn::orphan` | Late call dropped at entry guard | Cross-shard hashing bug, severe fan-in jitter, replay-with-time-skew |
| `worker::turn::no_user_start` | Partition discarded for lack of user-start call | Lost capture window at session boundary; orphan sub-agent traffic; profile mis-classifying user-start |
| `worker::turn::fin_idle` | Turn closed by idle timeout, not by terminal call | Truncated capture, client crash, missing terminal signal in profile |
| `worker::turn::fin_grace` | Turn closed normally via grace expiry | Healthy steady-state path; ratio vs `fin_idle` is the health signal |

## Edge Cases

1. **Cross-connection turns (Anthropic):** Same session sends calls over different TCP connections. Must group by `session_id`, not by TCP connection (client_ip:client_port).

2. **No finish_reason (truncated capture):** SSE stream cut off before `message_delta`. The call sits in the buffer; the buffer falls through to the idle sweep and emits `Incomplete` (or discards if no user-start landed).

3. **HTTP errors mid-turn:** `Error` and `Cancelled` are excluded from `is_main_terminal`, so they don't trigger grace. The call stays buffered; a retry with the same session joins the same partition. Without a retry, idle sweep finalizes as `Incomplete`.

4. **Multiple turns on same connection (Anthropic):** capture4 shows 2 turns on 1 connection. The `end_turn` response marks the boundary; the next request starts a new turn.

5. **No client headers (generic clients):** Without `X-Claude-Code-Session-Id` or `X-Codex-Turn-Metadata`, fall back to per-connection + finish_reason. Accept that cross-connection turns won't be detected.

6. **Parallel calls within a turn:** Some agents may fire multiple LLM calls concurrently (e.g., OpenAI Codex review running parallel sub-agents). These share the same `turn_id` and should all be grouped.

## Data Model

`LlmCall` is provider-shaped raw data. Turn association data lives in
`CallIdentity`, attached by `ts-llm/src/stage.rs` before the call enters the
turn shard:

- `profile_name: &'static str` — selects the `ClientProfile` impl
- `client_kind: String` — denormalized for storage (e.g. `claude-cli`)
- `session_id: String` — extracted by the profile
- `turn_id_hint: Option<String>` — set by Codex; informational only

`LlmCall.finish_reason: Option<FinishReason>` carries the normalized
terminal signal (see table above).

The aggregated `LlmTurn` (see `ts-turn/src/model.rs`) is built at finalize
time from the sorted partition:

- `turn_id: String` (UUIDv7), `session_id`, `stream_id`, `client_kind`,
  `provider`, `tenant_id`
- `start_time_us`, `end_time_us`, `duration_ms`, `call_count`, `call_ids`
- `models_used`, `subagents_used`
- `total_input_tokens`, `total_output_tokens`,
  `total_cache_read_input_tokens`, `total_cache_creation_input_tokens`
- `status: TurnStatus` — Complete / Length / Cancelled / Failed / Incomplete
- `final_finish_reason`, `user_input_preview`, `user_call_id`,
  `final_answer_preview`, `final_call_id`
