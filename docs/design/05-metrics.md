# Metrics Module Design

## Overview

The `ts-metrics` crate receives `CallStart` events and `LlmCall` records from the pipeline, aggregates them by time window and dimension combination, and outputs `LlmMetric` records for storage. `CallStart` enables precise realtime concurrency tracking. It is pure computation with no DB dependency.

## Aggregation Model

```
CallStart stream ──┐
                    ├──▶ MetricsAggregator ──▶ Vec<LlmMetric> ──▶ storage
LlmCall stream   ──┘
```

For each incoming event:
- `CallStart`: update concurrency counter (+1)
- `LlmCall`: update concurrency counter (-1), update counters + sketches in dimension buckets
- On window close: flush `LlmMetric` records and reset

## Dimensions

Each record is aggregated into multiple dimension combinations:

- `(provider, model, server_ip)` — finest pre-aggregated level
- `(provider, model, *)` — per-model across all servers
- `(*, *, server_ip)` — per-server across all models
- `(*, *, *)` — global, for overview dashboards

`*` means "all". Additional dimensions (tenant_id, etc.) will be added as they are validated with real traffic. Until then, per-tenant analysis queries the `llm_calls` detail table directly.

## Time Windows

Multiple granularities run in parallel:

| Granularity | Use case | Flush interval |
|-------------|----------|----------------|
| 10s | Realtime dashboard | every 10s |
| 1m | Recent trends | every 1m |
| 5m | Mid-term trends | every 5m |
| 1h | Historical analysis | every 1h |

## Per-Window State

For each (granularity × dimension combination), the aggregator maintains:

```rust
struct WindowBucket {
    // Traffic
    request_count: u64,
    stream_count: u64,
    non_stream_count: u64,

    // Concurrency (sampled per second)
    concurrency_samples: Vec<u32>,   // per-second snapshots
    concurrency_max: u32,

    // Tokens
    total_input_tokens: u64,
    total_output_tokens: u64,
    input_tokens_sketch: TDigest,

    // Errors
    error_count: u64,
    error_4xx_count: u64,
    error_429_count: u64,
    error_5xx_count: u64,

    // Performance sketches
    ttfb_sketch: TDigest,
    e2e_sketch: TDigest,

}
```

On window close, each bucket is flushed to an `LlmMetric` record with computed percentiles (avg/p50/p95/p99), then the bucket is reset.

## Concurrency Tracking

Concurrency cannot be derived from request counts — it requires tracking overlapping request lifespans.

The aggregator receives two types of events from `ts-llm`:
- **CallStart**: emitted when request headers are parsed and provider/model identified (carries timestamp, provider, model, is_stream)
- **CallEnd (LlmCall)**: the completed call record

On `CallStart` → counter +1 (per dimension bucket). On `CallEnd` → counter -1. Per-second snapshot within the window.

This enables per-dimension concurrency tracking (e.g. concurrent requests per model) from the moment a request arrives, not just after it completes.

On window close:
- `concurrency_avg` = mean of per-second samples
- `concurrency_max` = max of per-second samples

## Derivable Metrics (not stored)

These are computed at query time from stored fields:

| Metric | Derivation |
|--------|-----------|
| QPS | `request_count / window_seconds` |
| Success rate | `1 - error_count / request_count` |
| Error rate | `error_count / request_count` |
| 429 rate | `error_429_count / request_count` |
| Throughput (tokens/s) | `total_output_tokens / window_seconds` |

## File Structure

```
ts-metrics/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── aggregator.rs       # MetricsAggregator — window management + flush
    ├── bucket.rs           # WindowBucket — per-window counters + sketches
    ├── concurrency.rs      # ConcurrencyTracker — request overlap counting
    └── model.rs            # LlmMetric output type
```
