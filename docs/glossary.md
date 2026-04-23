# Glossary

This page explains the terms you will encounter in the TokenScope console and reports. It is written for a non-engineering reader — you should be able to follow it without knowing how the capture pipeline works. Pipeline-internal mechanics live in the per-module design docs under [design/](design/).

## Performance Metrics

The numbers shown on dashboards and detail views. Together they answer: *is the LLM service fast, stable, and efficient?*

**Aggregation unit — LLM Call, not HTTP Exchange.** Every metric in this section is computed over **LLM Calls**. An HTTP Exchange that TokenScope did not recognise as an LLM call (a health check, an admin endpoint, a misrouted request) does **not** contribute to Call Rate, Token Throughput, Call Error Rate, or any other number below. This keeps dashboards focused on inference traffic. If you need raw-HTTP counts for debugging, query the `http_exchanges` table directly. Agent-level equivalents (per-turn latency, per-turn token cost) are derived separately from Agent Turns and appear under the turn views, not here.

### TTFT — Time To First Token

**Definition:** Time from when the request hits the server to when the first token of the response is emitted. Unit: milliseconds.

**Why it matters:** TTFT is the user-perceived "it started typing" latency. For streaming calls (chat UIs, agent tools) it directly shapes how responsive the product feels. Regressions here usually point to the model being busy digesting long prompts, not to decode speed.

### E2E Latency — End-to-End Latency

**Definition:** Wall-clock time from the request being issued to the response being fully finished. Unit: milliseconds.

**Why it matters:** The total "how long did this cost me" number. For non-streaming calls, E2E is what the client actually waits for. Paired with TTFT, it tells you how much of the latency is front-loaded (processing the prompt) versus spent generating output.

### TPOT — Time Per Output Token

**Definition:** Average time to generate each output token once generation has started. Defined only for streaming responses. Unit: milliseconds per token.

**Why it matters:** Steady-state generation cost, independent of prompt length. TPOT is the cleanest view of raw inference speed per request — it is what operators compare across models and hardware.

### Token Throughput

**Definition:** Output tokens generated per second across a time window, summed over all LLM Calls. Unit: tokens per second.

**Why it matters:** Measures the deployment's overall token-producing capacity, not per-call speed. A single slow call has the same TPOT as a fast one but drags Token Throughput down if many run in parallel. The primary signal for capacity planning and for judging whether a deployment is saturated.

### Call Rate

**Definition:** Number of LLM Calls completed per second over a time window (call count divided by window length). Unit: calls per second. Roughly analogous to QPS in traditional web monitoring, but scoped to LLM traffic specifically.

**Why it matters:** Traffic volume. The classic "how busy is the service" number, read alongside Active Calls and Call Error Rate to understand load. Because it counts LLM Calls and not HTTP Exchanges, an agent turn with 20 tool-use round trips registers as 20 on Call Rate, not 1.

### Active Calls

**Definition:** Number of **LLM Calls** in flight at the same time — counted from the moment the request enters the server to the moment its response finishes. Reported as average and peak over a time window. This is **not** TCP-connection concurrency or HTTP-Exchange concurrency: one long-lived HTTP/2 connection that is actively serving three streaming completions contributes 3 to Active Calls, not 1.

**Why it matters:** Directly bounded by inference capacity. Watching Active Calls against latency reveals queuing: rising Active Calls with flat Call Rate is the classic "it is getting slower because demand outstripped supply" signature. Peak Active Calls is also the honest benchmark for sizing GPU/accelerator pools — it tells you the worst case the deployment actually handled.

### Cache Hit Ratio

**Definition:** Share of input tokens served from the provider's prompt cache instead of being re-processed. Unit: ratio from 0 to 1.

**Why it matters:** Prompt-cached tokens are billed at a steep discount (roughly 10% on Anthropic, 50% on OpenAI). A high hit ratio means lower spend for the same workload; a sudden drop usually means a prompt-template change broke cache reuse.

### Call Error Rate

**Definition:** Fraction of LLM Calls ending in HTTP 4xx/5xx status. The 429 sub-rate is tracked separately because it specifically signals rate-limit pressure.

**Why it matters:** Service-health headline. 429-heavy traffic means you have hit a provider quota; 5xx-heavy traffic means the provider itself is in trouble. Both are first-order reliability signals.

## Data Entities

The records TokenScope stores and that the console lets you drill into.

### HTTP Exchange

**Definition:** One paired HTTP request and response observed on the wire, with timings, status, sizes, and (for streaming responses) per-stream event counts. The record is API-agnostic — any HTTP traffic produces an exchange, whether or not it is an LLM call.

**Why it matters:** The ground-truth network-layer record. Every LLM Call is derived from exactly one HTTP Exchange, so when an LLM-level number looks wrong the exchange is where you verify what actually crossed the wire. It is also where non-LLM traffic (health checks, admin APIs, misrouted requests) shows up for diagnostic purposes.

### LLM Call

**Definition:** One request/response pair made to an LLM API — model, prompt, completion, token counts, latencies, status, and the full bodies on both sides. Derived from a single HTTP Exchange plus wire-API-specific parsing.

**Why it matters:** The atomic unit behind every chart. Aggregated metrics, agent turns, and billing views are all derived from calls. When you click "view raw exchange" in the console, an LLM Call is what you see.

### Agent Turn

**Definition:** One user-to-agent interaction cycle — from the moment the user submits a prompt to the moment the agent finishes replying. A turn groups every LLM Call made during that cycle under a single `turn_id` and `session_id`.

**What a turn actually looks like:** Modern coding agents (Claude Code, Codex CLI, Cursor) do not answer a user question with one LLM call. They loop: the model plans, asks for a tool, waits for the result, then plans again — often for dozens of round trips before finishing. A typical turn for a question like *"find the bug in auth.rs"* unfolds roughly like this:

1. User submits the prompt; agent opens the turn.
2. **LLM Call #1** — agent sends the prompt plus its tool catalogue. Model responds with `finish_reason = tool_use`, asking for `read_file(auth.rs)`.
3. Agent runs the tool locally, appends the file contents to the conversation history.
4. **LLM Call #2** — agent re-sends the grown conversation. Model responds with another `tool_use`, asking for `grep("validate_token")`.
5. Agent runs grep, appends the result.
6. **LLM Call #3 … #N−1** — more `tool_use` calls as the model reads further files, runs tests, or inspects state.
7. **LLM Call #N** — model finally returns a natural-language answer with `finish_reason = complete`. **This closes the turn.**

A single turn therefore contains anywhere from 1 LLM Call (simple Q&A) to 50+ (tool-heavy debugging or refactoring). The turn is closed by the **terminal call** — the one where the model stops asking for tools and returns an answer to the user.

**Why it matters:** Per-call metrics fragment the picture — a 30-call turn produces 30 different TTFTs, 30 finish reasons, 30 token counts. But the user only experienced one wait, paid for one task, and remembers one outcome. Per-turn metrics reflect that user-visible unit of work, which is what product, ops, and finance teams ultimately care about. Agent Turns are only produced for recognised agent clients (see **agent_kind**); traffic from unknown clients still generates LLM Calls but no Turn.

## Classification Axes

Two independent filters for slicing the data.

### Wire API

**Definition:** The HTTP API shape — method, path, body schema — the request uses. Current values include `anthropic`, `openai-chat`, and `openai-responses`.

**Why it matters:** The wire API is the *protocol*, not the *vendor*. Azure OpenAI, vLLM, Ollama, and OpenAI itself all speak `openai-chat`. Filtering by wire API compares like-for-like across deployments that share the same interface.

### Agent Kind

**Definition:** Short identifier for the *client* making the request, recognised from request headers. Examples: `claude-cli`, `codex-cli`.

**Why it matters:** Tells you *who* is calling, not *what* they are calling. Useful for separating traffic from your own agent products, internal tooling, and direct API users. Populated only when the client announces itself explicitly in headers.

## Key Fields

Identifiers and status fields that appear on the entities above and are worth understanding in their own right.

### session_id

**Definition:** Identifier of one agent-client session — the same value ties together every request made inside a single conversation, project, or IDE window of an agent product.

**How it is generated:** TokenScope does **not** invent this — it reads it directly off the wire, from whatever field the client itself already uses. Each recognised agent kind has a different source:

- `claude-cli` (Claude Code): the `X-Claude-Code-Session-Id` request header.
- `codex-cli` (Codex CLI): the `session_id` field in the request body.

**Why it matters:** Groups all LLM Calls that belong to the same user conversation, even when they are spread across many HTTP connections. It is the join key behind Agent Turns and the primary pivot for per-user and per-conversation analysis.

### turn_id

**Definition:** Identifier of one Agent Turn (one user-to-agent interaction cycle).

**How it is generated:** Depends on whether the client exposes turn boundaries itself:

- **Client provides one (Codex CLI):** the client sends an explicit turn identifier on every call in the turn; TokenScope captures it verbatim.
- **Client does not (Claude Code / Anthropic):** TokenScope's turn tracker detects the start of a new turn from the call pattern and mints a UUIDv7, then stamps it onto every call in that turn until the terminal call closes it. UUIDv7 is time-ordered, so turn IDs sort naturally by when the turn happened.

**Why it matters:** Stable back-reference from every LLM Call to the interaction cycle it belongs to. Without it, per-turn metrics, drill-down views, and session timelines cannot be reconstructed.

### finish_reason

**Definition:** Normalised reason the LLM Call ended. Five possible values:

| Value | Meaning |
|-------|---------|
| `complete` | Model finished generating naturally. |
| `length` | Hit the max-output-tokens limit. |
| `tool_use` | Model decided to call a tool; generation paused awaiting tool result. |
| `error` | Call failed (HTTP error or provider-side failure). |
| `cancelled` | Client disconnected or aborted before completion. |

**How it is generated:** Each wire API reports termination differently; TokenScope normalises them into this common vocabulary. Example mappings:

- OpenAI `finish_reason: "stop"` → `complete`; `"length"` → `length`; `"tool_calls"` → `tool_use`.
- Anthropic `stop_reason: "end_turn"` → `complete`; `"max_tokens"` → `length`; `"tool_use"` → `tool_use`.

For streaming responses the value is taken from the terminal SSE event; for non-streaming, from the response body.

**Why it matters:** The single most useful field for triage. A spike in `length` means prompts or outputs need re-tuning; a spike in `error` or `cancelled` points at infrastructure or client-side problems; the `tool_use` share reveals how agent-heavy the workload is.

## Streaming & Caching

### Streaming (SSE)

**Definition:** Server-Sent Events — the HTTP streaming format (`text/event-stream`) all major LLM APIs use to deliver token-by-token responses.

**Why it matters:** Streaming is why TTFT and TPOT exist as distinct metrics. For non-streaming calls these two numbers collapse back into E2E — the client simply waits for the full response.

### Prompt Cache

**Definition:** Provider-side reuse of previously-seen prompt prefixes. When the provider has already processed a prefix, billing drops and TTFT improves.

**Why it matters:** For agent workloads that re-send large system prompts or tool schemas on every turn, prompt caching is the single biggest lever on both cost and latency. Track the cache hit ratio when tuning prompts or rolling out template changes.
