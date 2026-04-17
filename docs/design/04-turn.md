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
- Turn boundaries: explicit terminal signal (stop_reason, status) OR new
  user-turn start (`messages[-1]` / `input[-1]` inspection) OR idle timeout
  (default 600 s, packet-time-driven).

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
1. Group by `X-Claude-Code-Session-Id` (if present)
2. Within a session, use `finish_reason` state machine:
   - New request + no active turn → start new turn (generate `turn_id`)
   - Response with `tool_use` → turn continues
   - Response with `end_turn` → turn ends, next request starts new turn
3. Without session header → fall back to per-connection grouping (may miss cross-connection turns)

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
1. Extract `turn_id` from `X-Codex-Turn-Metadata` header or request body → direct grouping (no state machine needed)
2. Without turn header → fall back to finish_reason analysis:
   - Response with only `function_call` outputs → turn continues
   - Response with `message` output → turn complete

### OpenAI Chat Completions API

Not yet observed in captures. Expected behavior based on API docs:
- `finish_reason: "tool_calls"` → agent continues
- `finish_reason: "stop"` → agent done
- Connection behavior varies by client

## Provider-Specific Extraction

Each provider's extractor is responsible for:
1. Extracting `session_id` and `turn_id` from headers/body (if available)
2. Normalizing `finish_reason` to indicate whether the turn continues or ends

| Provider | session_id source | turn_id source | Turn boundary |
|----------|------------------|----------------|---------------|
| Anthropic | `X-Claude-Code-Session-Id` header | Generated (not in protocol) | `stop_reason` state machine |
| OpenAI Responses | `session_id` in body/header | `turn_id` in `X-Codex-Turn-Metadata` | Explicit `turn_id` grouping |
| OpenAI Chat | `Authorization` token prefix | Generated (not in protocol) | `finish_reason` state machine |

## FinishReason Normalization

The `FinishReason` enum serves as a unified turn-continuation signal:

| FinishReason | Meaning | Anthropic source | OpenAI Chat source | OpenAI Responses source |
|-------------|---------|-----------------|-------------------|------------------------|
| `ToolUse` | Agent continues | `stop_reason: "tool_use"` | `finish_reason: "tool_calls"` | output contains only `function_call` items |
| `Complete` | Turn ends | `stop_reason: "end_turn"` | `finish_reason: "stop"` | output contains `message` item |
| `Length` | Max tokens hit | `stop_reason: "max_tokens"` | `finish_reason: "length"` | `status: "incomplete"` |
| `Error` | Generation error | (HTTP error) | (HTTP error) | `status: "failed"` |
| `Cancelled` | User cancelled | (connection close) | `finish_reason: "content_filter"` | `status: "cancelled"` |

## Turn State Machine (Generic Fallback)

When no explicit `turn_id` is available, use this state machine per session (or per connection if no session header):

```
                    ┌──────────────────────────────┐
                    │                              │
                    ▼                              │
  Idle ──[request]──▶ InTurn ──[resp: ToolUse]─────┘
                        │
                        ├──[resp: Complete]──▶ Idle  (emit Turn)
                        ├──[resp: Length]────▶ Idle  (emit incomplete Turn)
                        ├──[resp: Error]────▶ InTurn (keep open, client may retry)
                        ├──[HTTP 4xx/5xx]───▶ InTurn (keep open, client may retry)
                        └──[timeout]────────▶ Idle  (emit incomplete Turn)
```

On turn start, generate a `turn_id` using: `turn-{timestamp_us}-{random_suffix}`.

## Edge Cases

1. **Cross-connection turns (Anthropic):** Same session sends calls over different TCP connections. Must group by `session_id`, not by TCP connection (client_ip:client_port).

2. **No finish_reason (truncated capture):** SSE stream cut off before `message_delta`. Keep turn open; close on timeout or EOF with status "incomplete."

3. **HTTP errors mid-turn:** 4xx/5xx response doesn't end the turn — the client may retry. Keep turn open.

4. **Multiple turns on same connection (Anthropic):** capture4 shows 2 turns on 1 connection. The `end_turn` response marks the boundary; the next request starts a new turn.

5. **No client headers (generic clients):** Without `X-Claude-Code-Session-Id` or `X-Codex-Turn-Metadata`, fall back to per-connection + finish_reason. Accept that cross-connection turns won't be detected.

6. **Parallel calls within a turn:** Some agents may fire multiple LLM calls concurrently (e.g., OpenAI Codex review running parallel sub-agents). These share the same `turn_id` and should all be grouped.

## Data Model

Each `LlmCall` carries:
- `session_id: Option<String>` — from client headers, for session-level grouping
- `turn_id: Option<String>` — explicit (from client headers) or generated (from state machine)
- `turn_index: Option<u32>` — sequence number within the turn (0, 1, 2, ...)
- `finish_reason: Option<FinishReason>` — normalized turn-continuation signal

The Turn itself is an aggregation (computed from grouped LlmCalls):
- `turn_id: String`
- `session_id: Option<String>`
- `call_count: u32`
- `total_input_tokens: u32`
- `total_output_tokens: u32`
- `start_time: Timestamp`
- `end_time: Timestamp`
- `status: TurnStatus` — Complete, Incomplete, InProgress
