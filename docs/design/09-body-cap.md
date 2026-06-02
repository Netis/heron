# 09 — Stored-body cap

> Bounds the size of request/response bodies Heron persists, so 1M-token
> contexts don't pressure capture-node memory. Implemented as a head + tail
> sampling policy applied **after** extraction. See issue H007.

## Problem

For each LLM call Heron stores the full request and response body
(`llm_calls.request_body` / `response_body`) for display and re-classification.
With 2026-era 1M-token context windows a single request body is routinely
multiple megabytes. Two failure modes follow:

1. **Capture-node memory pressure.** The storage write buffer batches ~1000
   rows (`[storage.sink].batch_size`) before flushing. At multi-MB per body
   that is gigabytes held in memory — on a node whose job is to *passively
   observe* without disturbing the host it runs on. An observer that OOMs the
   capture node violates Heron's core non-intrusiveness guarantee.

2. **Silent metric loss (the original framing).** The only pre-existing byte
   bound was `snaplen` (262144, per-*packet*), which truncates reassembly
   rather than bounding the accumulated body. A snaplen-truncated tail can drop
   the trailing `usage` block on a large non-streaming response. `snaplen` is
   owned by `h-capture` and is explicitly out of scope here.

## What the cap does

`h_common::config::BodyCapConfig` (`[body_cap]` in `default.toml`):

| field        | default   | meaning                                            |
|--------------|-----------|----------------------------------------------------|
| `enabled`    | `true`    | master switch; `false` ⇒ unbounded (legacy)        |
| `head_bytes` | `262144`  | bytes retained from the start of each body         |
| `tail_bytes` | `65536`   | bytes retained from the end of each body           |

A body larger than `head_bytes + tail_bytes` is stored as **first `head_bytes`
+ elision marker + last `tail_bytes`**; the middle is dropped and the dropped
byte count is recorded in `llm_calls.body_bytes_dropped` (UBIGINT, default 0).
A body at or below the budget is stored verbatim with `body_bytes_dropped = 0`.

- The **head** holds model, parameters, system prompt, tool schemas, and the
  first messages.
- The **tail** holds the final messages and, on non-streaming responses, the
  trailing `usage` block.
- Window boundaries are snapped to UTF-8 char boundaries so the stored `String`
  stays valid.

The cap is **symmetric** (same budget for request and response). The config
struct is shaped so a separate response budget can be added later without
breaking existing files.

## Where it runs — and why not in `BodyReader`

H007's suggested approach was to cap inside `h-protocol`'s `BodyReader` at
accumulation time. We deliberately do **not** do that in this iteration.

Extraction is not tolerant of a sampled body: `h-llm` parses the *full* body as
JSON (`ParsedJson` → `serde_json::from_slice`) to read usage, model, and agent
primitives. A head+tail body with the middle elided is not valid JSON, so
capping before extraction would break usage/model extraction — exactly what
H007's success criteria forbid. H007's own "tail-aware sampling that parses the
body to locate the usage event" is explicitly **deferred**.

So the cap is applied at the **storage boundary**, in
`h_llm::processor::cap_body`, immediately before the `LlmCall` is built and
*after* every extraction (wire detection, usage/model, token estimation, agent
classification) has run on the full body. This:

- keeps usage/model/agent accuracy at 100% (they never see the capped body);
- bounds the **sustained** memory consumer — the storage write buffer and the
  persisted rows — which is the dominant risk at scale.

```
h-protocol (full body) ─▶ h-llm extract (full body) ─▶ cap_body ─▶ LlmCall ─▶ write buffer ─▶ DuckDB
                                                          ▲ head+tail only from here on
```

### Known limitation (follow-up)

This policy does **not** bound the *transient* peak: a single in-flight body is
still fully reassembled in `h-protocol` and held through the protocol→joiner→
processor channels before `cap_body` trims it. That peak is bounded by the
flow-shard count, not by `head_bytes + tail_bytes`. Bounding the transient peak
requires capping in `BodyReader` *and* a tail-aware usage parser that can read
`usage` from a sampled body — H007's deferred item, tracked for a future
iteration.

## Storage

`llm_calls.body_bytes_dropped UBIGINT NOT NULL DEFAULT 0`, at the table tail.
The Phase-6 migration adds it to legacy DBs via
`ALTER TABLE llm_calls ADD COLUMN IF NOT EXISTS body_bytes_dropped UBIGINT
DEFAULT 0` — **without `NOT NULL`**, because DuckDB rejects
`ALTER ... ADD COLUMN ... NOT NULL DEFAULT` (see `07-schema.md` and the Phase-5
note in `schema.rs`). The column sits last in both `CREATE TABLE` and the
`ALTER`, so the positional appender stays aligned on fresh and migrated DBs
alike. `tests/migrations.rs` covers the migration *and* writes a row through
the appender to assert the column round-trips (alignment regression guard).
