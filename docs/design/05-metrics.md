# Metrics Module Design

## Overview

The `ts-metrics` crate receives `LlmEvent` values from the pipeline, aggregates them by time window and dimension combination, and emits `LlmMetric` rows for storage. It is pure computation with no DB dependency.

## Event Inputs

The aggregator consumes three kinds of `LlmEvent`:

- **`Start`** — emitted when request headers are parsed. Carries `source_id`, timestamp, `wire_api`, `model`, `is_stream`, `server_ip`. Writes Start-side fields (traffic counts, active-calls sample) into the bucket.
- **`Complete`** — emitted when the full LLM call has been assembled. Carries the full `LlmCall`. Writes Complete-side fields (tokens, errors, finish reason, TTFT / E2E / TPOT samples) into the bucket.
- **`Heartbeat`** — synthetic event-time advance, broadcast from capture to every shard. Does not write data; only advances the per-source watermark so the drain cadence fires on idle sources.

## Aggregation Model

```
LlmEvent::Start     ──┐
LlmEvent::Complete  ──┼──▶ MetricsAggregator ──▶ Vec<LlmMetric> ──▶ storage
LlmEvent::Heartbeat ──┘
```

For each `(source_id, granularity, window_start(request_time), dim)` key the aggregator owns one `WindowBucket`. Both Start and Complete for the same call key by `window_start(request_time)`, so a late Complete always lands in the same window as its originating Start — the口径 is strictly request-time.

## Drain Cadence (not watermark close)

Buckets do not close the instant the watermark crosses `window_end`. Each `(source, granularity)` pair owns a **drain anchor** (`last_flush_ts`) that is initialized on the first bucket write to `window_start(ts, gran)`. On every processed event the aggregator checks, for each granularity: if `watermark - last_flush_ts ≥ gran.window_secs` (event-time), every non-empty bucket for that `(source, gran)` is flushed and removed, and the anchor advances to the current watermark.

Two consequences:

- **No start-vs-complete window-split.** Fast responses (Start + Complete within the same cadence slice) produce **one** merged row per `(window, dim)`. Start-side and Complete-side fields are written into the same bucket because they are disjoint field sets.
- **Late Complete never lost.** A response arriving after its window has already been drained opens a fresh bucket at the same `window_start(request_time)`, carrying only Complete-side fields. It is emitted at the next cadence as an additional row; query-time SUM reassembles the full window.

First-drain alignment via the `window_start` anchor prevents a single early event from emitting a one-sample row the moment it arrives mid-window.

## Dimensions

Each record is aggregated into four dimension combinations:

- `(wire_api, model, server_ip)` — finest pre-aggregated level
- `(wire_api, model, *)` — per-model across all servers
- `(*, *, server_ip)` — per-server across all models
- `(*, *, *)` — global, for overview dashboards

`*` means "all". Per-tenant analysis queries the `llm_calls` detail table directly for now.

## Time Windows

Multiple granularities run in parallel, each with its own cadence:

| Granularity | Use case | Drain cadence (event-time) |
|-------------|----------|----------------------------|
| 10s | Realtime dashboard | 10s |
| 1m  | Recent trends | 60s |
| 5m  | Mid-term trends | 300s |
| 1h  | Historical analysis | 3600s |

Per-granularity cadence keeps row counts bounded: in the fast-response steady state each `(window, dim)` emits exactly one row (same as before the refactor); slow responses that straddle a cadence boundary add one extra row per crossed cadence.

## Per-Window State

```rust
pub struct WindowBucket {
    // Start-side (from LlmEvent::Start + active-calls sampling)
    call_count: u64,
    stream_count: u64,
    non_stream_count: u64,
    active_calls_sum: u64,          // Σ samples, for exact avg via SUM() at query time
    active_calls_sample_count: u64,
    active_calls_max: u32,

    // Complete-side (from LlmEvent::Complete)
    total_input_tokens: u64,
    input_token_count: u64,
    total_output_tokens: u64,
    output_token_count: u64,
    total_cache_read_input_tokens: u64,
    total_cache_creation_input_tokens: u64,

    error_count: u64,
    error_4xx_count: u64,
    error_429_count: u64,
    error_5xx_count: u64,

    finish_complete_count: u64,
    finish_length_count: u64,
    finish_tool_use_count: u64,
    finish_error_count: u64,
    finish_cancelled_count: u64,

    // Latency: exact running sum+count + t-digest for per-row percentiles
    ttft: DistributionDigest,  // sum, count, p50/p95/p99
    e2e:  DistributionDigest,
    tpot: DistributionDigest,  // streaming only
}
```

`DistributionDigest` tracks `sum` and `count` exactly (untouched by digest compaction) so query-time `SUM(ttft_sum) / SUM(ttft_count)` produces an exact average over any multi-row aggregation. Percentiles are per-row t-digest estimates over that row's slice; cross-row aggregation is a weighted average by the row's `*_count` (approximate until the schema adopts serialized t-digest bytes).

## Active Calls Tracking

The **Active Calls** metric (in-flight LLM call count) requires overlapping-call counting, not a post-hoc derivation.

- On `Start`: aggregator increments the per-`DimensionKey` active-calls counter; the bucket writes the current value into `active_calls_sum / active_calls_sample_count` and updates `active_calls_max`.
- On `Complete`: the per-`DimensionKey` counter is decremented (floored at 0). No sample is recorded for the Complete side.

Per-row avg is `active_calls_sum / active_calls_sample_count`; cross-row avg is `SUM(active_calls_sum) / SUM(active_calls_sample_count)`. `active_calls_max` uses `MAX()` across rows.

## Per-Source Watermark

`latest_ts[source_id]` tracks the maximum event-time seen for that source. Advanced by:

- `Start.timestamp_us`
- `Complete.complete_time.unwrap_or(request_time)`
- `Heartbeat.ts`

A busy source advances its own watermark without needing heartbeats; heartbeats only matter for sources that would otherwise be idle between events. One source's watermark never advances another's — session isolation across sources is preserved.

## Derivable Metrics (not stored)

| Metric | Derivation |
|--------|-----------|
| Call Rate | `call_count / window_seconds` |
| Call Success Rate | `1 - error_count / call_count` |
| Call Error Rate | `error_count / call_count` |
| Call 429 Rate | `error_429_count / call_count` |
| Token Throughput (tokens/s) | `total_output_tokens / window_seconds` |
| Cache Hit Ratio | `total_cache_read_input_tokens / total_input_tokens` |

## File Structure

```
ts-metrics/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── aggregator.rs      # MetricsAggregator — per-source watermark, cadence drain
    ├── bucket.rs          # WindowBucket + DistributionDigest
    ├── stage.rs           # Tokio-task wiring (one aggregator per shard)
    └── model.rs           # LlmMetric + derived avg accessors
```
