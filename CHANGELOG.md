# Changelog

All notable changes to TokenScope are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking changes

- **`llm_calls.finish_reason` value space.** Changed from normalized labels (`complete` / `length` / `tool_use` / `error` / `cancelled`) to **raw provider values** verbatim. Per-provider vocabulary:
  - Anthropic: `end_turn`, `stop_sequence`, `max_tokens`, `tool_use`, `pause_turn`, `refusal`, `model_context_window_exceeded`
  - OpenAI Chat: `stop`, `length`, `tool_calls`, `function_call`, `content_filter`
  - OpenAI Responses: `completed`, `incomplete`, `failed`, `cancelled`

  Interpret using the row's `wire_api`. Historical rows written before this version still carry legacy normalized values; queries that mix old and new data must handle both. **No reverse migration is performed.**
- **`agent_turns.status` collapsed to binary** (`complete` | `incomplete`). Former `length` / `failed` / `cancelled` values are gone — the wire-level reason is preserved verbatim in `final_finish_reason`. Existing databases get a one-time `UPDATE` on init: `length → complete`, `failed`/`cancelled → incomplete`.
- **`llm_metrics` table dropped 5 `finish_*_count` columns.** Replaced by new long-format table `llm_finish_metrics(timestamp, source_id, granularity, wire_api, model, server_ip, finish_reason, count)`. Existing databases drop the columns via idempotent `ALTER TABLE ... DROP COLUMN IF EXISTS` on init; no historical backfill into the new table.
- **`GET /api/llm-calls?errors_only=true` removed.** Filter by `status_code` for HTTP errors and/or `finish_reason` for specific provider outcomes (`refusal`, `content_filter`, `failed`, etc.).
- **`GET /api/metrics/finish-reasons` added.** Returns long-format finish-reason timeseries. Query params: `granularity`, `start`, `end`, optional CSV `wire_api`, optional CSV `model`. Response: `{ series: [{ finish_reason: string, points: [[ts_us, count], ...] }, ...] }`.
- **Console UI:**
  - "Errors only" toggle removed from `/calls` page.
  - Finish-reason filter dropdown regrouped by `wire_api` (Anthropic / OpenAI Chat / OpenAI Responses) listing each endpoint's raw values.
  - Stale URL params (e.g. `?status=length`, `?finish=complete` from old bookmarks) silently filtered against the new vocabulary.
  - Traffic chart on `/traffic` renders one series per raw provider value with tone-based colors.

### Fixed

- Anthropic `pause_turn` no longer counted as an error and no longer prematurely closes the agent turn (was previously bucketed into `FinishReason::Error` via a catch-all and treated as terminal by the tracker).
- Anthropic `refusal` and `model_context_window_exceeded` now persist verbatim with their wire-level semantics intact instead of being silently bucketed into `error` / `length`.
- OpenAI Chat `content_filter` no longer maps to `cancelled` (which was inaccurate — content filtering is a wire-level terminal, not a user cancellation).

### Internal

- New `WireApi::is_terminal(&str) -> bool` and `WireApi::is_tool_use(&str) -> bool` trait methods centralize provider vocabulary semantics. Consumers (tracker, metrics, UI) MUST route through these predicates rather than hardcoding string comparisons.
- Tracker `is_main_terminal` predicate routes through `WireApi` dispatch instead of matching against the deleted `FinishReason` enum.
- `WindowBucket.finish_counts: BTreeMap<String, u64>` (previously 5 fixed `finish_*_count: u64` fields).
- Frontend `lib/finish-tone.ts` maps raw values to 6 visual tones (`ok` / `warn` / `tool` / `pause` / `err` / `muted`) — reused by `FinishBadge`, call-card, gantt, traffic chart.
