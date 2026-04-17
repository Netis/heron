# Internal Metrics Design

## Overview

Self-monitoring module for diagnosing pipeline bottlenecks. Each processing stage maintains lightweight counters; a collector periodically samples and logs them to console. This is separate from `ts-metrics` (which aggregates LLM business metrics).

## Counters per Stage

| Stage | Counters |
|-------|----------|
| **capture::pcap** | packets_received, packets_dropped (libpcap stats) |
| **capture::cloud_probe** | batches_received, packets_received |
| **flow_dispatcher** | packets_dispatched, per-worker queue depth |
| **worker::net** | packets_parsed, tcp_flows_active, tcp_flows_completed |
| **worker::http** | requests_parsed, responses_parsed, sse_events_parsed |
| **worker::llm** | calls_extracted, calls_failed |
| **metrics::aggregator** | events_received (CallStart + CallEnd), windows_flushed |
| **storage::buffer** | records_buffered, records_flushed, flush_errors |

Each counter is an `AtomicU64`, incremented in the hot path with zero allocation.

## Collector

A background Tokio task runs on a configurable interval (default: 10s):

1. Read all counters
2. Compute delta since last sample
3. Log to console via `tracing::info!`

Output format (total/delta since last sample):
```
[INTERNAL] capture::pcap        packets_received=125000/12500 packets_dropped=0/0
[INTERNAL] worker::0::net       packets_parsed=62000/6200 tcp_flows_active=150
[INTERNAL] worker::0::http      requests_parsed=3100/310 sse_events_parsed=45000/4500
[INTERNAL] worker::0::llm       calls_extracted=3100/310 calls_failed=2/0
[INTERNAL] metrics              events_received=6200/620 windows_flushed=4/1
[INTERNAL] storage              records_buffered=3100/310 records_flushed=3000/3000
```

## Configuration

```toml
[internal_metrics]
enabled = true
interval_secs = 10
```

## Location

Not a separate crate. Lives in `ts-common` as a shared utility, since all crates need to register and increment counters.

```
ts-common/src/
    ├── ...
    └── internal_metrics.rs   # AtomicU64 counters + collector + console logger
```
