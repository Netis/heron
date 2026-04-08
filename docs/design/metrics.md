# Metrics Module Design

## Overview

The `ts-metrics` crate receives `LlmRequest` records from the pipeline, aggregates them by time window and dimension combination, and outputs `LlmMetric` records for storage. It is pure computation with no DB dependency.

## Aggregation Model

```
LlmRequest stream
       │
       ▼
  ┌─────────────────────────────────────┐
  │          MetricsAggregator          │
  │                                     │
  │  For each request:                  │
  │    1. Determine dimension bucket    │
  │    2. Update counters + sketches    │
  │                                     │
  │  On window close:                   │
  │    3. Flush LlmMetric records       │
  └─────────────────────────────────────┘
       │
       ▼
  Vec<LlmMetric> ──▶ storage
```

## Dimensions

Each `LlmRequest` is aggregated into multiple dimension combinations simultaneously:

- `(provider, model, tenant_id, server_node)` — most specific
- `(provider, model, tenant_id, *)` — per-tenant per-model
- `(provider, model, *, *)` — per-model
- `(*, *, *, *)` — global

`*` means "all". This pre-computes common query patterns so dashboards don't need runtime GROUP BY.

## Time Windows

Multiple granularities run in parallel:

| Granularity | Use case | Flush interval |
|-------------|----------|----------------|
| 10s | Realtime dashboard | every 10s |
| 1m | Recent trends | every 1m |
| 5m | Mid-term trends | every 5m |
| 1h | Historical analysis | every 1h |

Coarser windows can be computed by re-aggregating finer ones (for counts and sums). Percentiles require independent computation at each granularity since sketches don't merge losslessly across time windows.

## Per-Window State

For each (granularity × dimension combination), the aggregator maintains:

```rust
struct WindowBucket {
    // Counters
    request_count: u64,
    error_count: u64,

    // Token sums
    total_input_tokens: u64,
    total_output_tokens: u64,

    // Percentile sketches
    ttfb_sketch: TDigest,
    tpot_sketch: TDigest,
    e2e_sketch: TDigest,
}
```

On window close, each bucket is flushed to an `LlmMetric` record with computed percentiles (avg/p50/p95/p99), then the bucket is reset.

## TPOT Computation

TPOT (Time Per Output Token) is derived per-request before feeding into the sketch:

```
tpot = (complete_time - response_time) / output_tokens
```

Requests without `output_tokens` or `response_time` are excluded from TPOT aggregation.

## File Structure

```
ts-metrics/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── aggregator.rs       # MetricsAggregator — window management + flush
    ├── bucket.rs           # WindowBucket — per-window counters + sketches
    └── model.rs            # LlmMetric output type
```
