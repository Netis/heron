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
| **worker::turn** | calls_ingested, calls_aux, completed, timed_out, orphan, fin_grace, fin_idle, no_user_start |
| **metrics::aggregator** | events_received (CallStart + CallEnd), windows_flushed |
| **storage::buffer** | records_buffered, records_flushed, flush_errors |

### `worker::turn` operator notes

The turn shard's buffer-and-finalize machinery (see `04-turn.md` and
`04b-turn-reorder-proposal.md`) emits these counters per shard:

| Short name | Semantics | What rising values usually mean |
|---|---|---|
| `calls_ingested` | Calls accepted into a SessionBuffer (or routed past it) | Should track `worker::llm::calls_extracted` minus aux |
| `calls_aux` | Auxiliary one-shots skipped (e.g. claude-cli session-title) | Steady-state baseline; no action needed |
| `completed` | Turns finalized and emitted downstream | Healthy throughput signal |
| `fin_grace` | Subset of `completed` closed via grace expiry (terminal observed) | Normal path; expect this to dominate |
| `fin_idle` | Subset of `completed` closed via idle timeout (no terminal observed) | Truncated capture, missing terminal predicate, or client crash. If `fin_idle` ≳ `fin_grace`, investigate profiles or capture window |
| `timed_out` | Same event as `fin_idle`, kept for back-compat dashboards | (Mirror of `fin_idle`) |
| `orphan` | Late call dropped at the buffer entry guard (request_time < high-water) | Severe fan-in jitter, broken sharding (same session crossing shards), or replay-with-time-skew |
| `no_user_start` | Partition discarded because no call carried `is_user_turn_start = Some(true)` | Lost capture window at session boundary; orphan sub-agent traffic; profile mis-classifying user-start. Small steady rate is normal during traffic ramp-up |

**Tuning `grace_ms`** (config: `[pipeline.turn] grace_ms`, default 1000): the
grace window is the only added per-call latency in the steady-state path. If
`orphan` rises in correlation with multi-connection sessions, raise `grace_ms`.
If turn-emit latency in dashboards is too high, lower it — but verify that
`no_user_start` and `orphan` don't climb in response.

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
