# Proposal 0002 — `LlmCall.exchange_id` FK + Raw HTTP viewer migration

**Status**: Draft
**Related**: Proposal 0001 (http-exchanges separation, referenced in
`ts-protocol/src/joiner.rs:8`)

## Problem

Today `llm_calls` and `http_exchanges` each store a copy of the same HTTP
payload bytes:

| Column                               | `llm_calls`      | `http_exchanges`   |
|--------------------------------------|------------------|--------------------|
| `request_body`                       | VARCHAR          | BLOB               |
| `response_body`                      | VARCHAR          | BLOB               |
| `request_headers` / `response_headers` | VARCHAR (JSON) | VARCHAR (JSON)     |

For non-SSE calls both copies hold the same bytes. The `llm_calls` copy is
also *mis-labelled* for SSE: `response_body` there is the **reconstructed**
JSON synthesized by `WireApi::extract_sse`, not the wire-level SSE bytes.
(Wire-level SSE bytes are intentionally discarded at parse time —
`joiner.rs:97-107`.)

`HttpJoiner` already mints a stable `UUIDv7` per exchange and the joiner
module's own doc comment names the intended FK:

> `id` is … the primary key for `http_exchanges` and the stable
> correlation id downstream (e.g. `LlmCall.exchange_id` FK).
> — `joiner.rs:46-47`

But `LlmCall` has no `exchange_id` column today, so the link exists only in
comments.

## Consequences of the current state

1. **Storage waste** — bodies duplicated across two tables.
2. **No independent retention** — `http_exchanges` is configured with a
   short retention (default `http_exchanges = 7` days in
   `server/config/default.toml:85`), but as long as bodies live on
   `llm_calls` the configuration is effectively dead. Bulky bytes stay
   alive for the long retention window of `llm_calls`.
3. **Drawer mis-nomer** — `<RawHttpDrawer>` claims "Raw HTTP" but for SSE
   calls it shows the reconstructed body, not the transport-layer record.

## Proposal

Add `LlmCall.exchange_id` as a stable FK to `http_exchanges.id`, migrate
the `View raw HTTP` drawer to read transport fields from
`http_exchanges`, and remove the redundant body/headers columns from
`llm_calls`.

### Wire-up in the pipeline

`HttpJoinerEvent::Exchange { id, request, response, sse_events }` already
carries the exchange id. Thread `id` into `LlmProcessor::on_exchange` (or
wherever `LlmCall` is constructed) and set it on `LlmCall.exchange_id`.
No new state, no new channel — just forward a field that already exists.

### API changes

- `CallDetail` replaces the four transport columns with `exchange_id:
  String`.
- `GET /api/llm-calls/{id}` optionally expands the linked exchange when
  the drawer asks for it (query flag, or a sibling endpoint
  `GET /api/llm-calls/{id}/exchange` returning `HttpExchangeDetail`).
  The latter is simpler — one request, one record, clear contract.

### Console changes

- `RawHttpDrawer` drops the `detail.request_body` / `detail.response_body`
  / `detail.request_headers` / `detail.response_headers` reads.
- Replaces them with `useHttpExchangeDetail(callId)` (new hook hitting
  the sibling endpoint).
- Graceful-degrade states:
  - **SSE call**: `response_body` is `NULL` — render "SSE response —
    body not retained at transport layer. Parsed view shown in the call
    card above."
  - **Exchange aged out**: endpoint returns 404 — render "Raw exchange
    no longer retained (past `http_exchanges` retention window)."

### Schema migration

1. Add `exchange_id VARCHAR` to `llm_calls` (nullable for existing rows).
2. Backfill: left blank for historical rows — they'll lose `View raw
   HTTP` after upgrade. Acceptable because historical rows often predate
   the `http_exchanges` table itself.
3. Follow-up migration drops `request_body`, `response_body`,
   `request_headers`, `response_headers` from `llm_calls`.

Do **1** and **2** in the same release; schedule **3** after one release
of dual-write (bodies continue to land in both tables) so rollback is
painless.

## Open questions

1. **SSE response in the drawer.** Two options:
   - **Accept the degradation** — "Raw HTTP" means raw. SSE has no raw
     response to show; drawer links to the parsed view on the call card.
     Simpler, fewer columns, consistent with the table's BLOB-null
     semantics.
   - **Keep a `reconstructed_response` column on `llm_calls`** — so the
     drawer still has something to display for SSE. This partially
     preserves the current UX but reintroduces a narrower form of the
     duplication we're trying to remove.
   - *Recommendation*: accept the degradation. Drawer shows headers +
     the non-SSE body; SSE responses get a contextual note and a deep
     link to the parsed view.
2. **Retention-miss UX.** Is a plain 404 → inline message sufficient, or
   do we want a preemptive "this exchange expires on DATE" hint in the
   drawer even while it's still alive? The latter requires plumbing the
   retention config into the API response.
3. **Historical data.** Do we backfill `exchange_id` from timestamp +
   (client, server) address tuples as a best-effort? Likely not worth
   the complexity — the correlation isn't guaranteed unique.

## Rollout plan

| Step | Change | Revertable? |
|------|--------|-------------|
| 1    | Add `LlmCall.exchange_id`, wire joiner id through, dual-write bodies | Yes — drop column |
| 2    | Migrate drawer to new endpoint; keep both API shapes temporarily | Yes — frontend flag |
| 3    | Drop body/headers columns from `llm_calls`            | No — data loss |

Step 3 is the irreversible one. Gate it on a conscious go/no-go review
after steps 1-2 have soaked.

## Non-goals

- Retaining raw SSE bytes. That would require reworking the SSE parser
  to keep a separate raw buffer — orders of magnitude more invasive and
  inflates `http_exchanges` size.
- Changing `http_exchanges` schema. Only `llm_calls` changes here.
