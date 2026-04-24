# Internal Metrics Design

## Overview

Self-monitoring module for diagnosing pipeline bottlenecks. Each processing stage maintains lightweight counters; a collector periodically samples and logs them to console. This is separate from `ts-metrics` (which aggregates LLM business metrics).

All metrics are defined in a single `define_metrics!` macro invocation in `ts-common/src/internal_metrics.rs` — the canonical source of truth. Each variant carries `kind` (Counter / Gauge), `group` (report grouping), and a `short` name used in the console output.

## Report format

One log line per group per interval. The line is prefixed by a `label` (pipeline name) supplied at reporter start:

```
[INTERNAL] <label> | <group>  | short=total/delta short=total/delta ... q.name=depth
```

Counters print as `total/delta`; gauges print as current value only.

Groups and their display order:

| Group    | Subject                                     |
|----------|---------------------------------------------|
| capture  | libpcap / cloud-probe source ingestion      |
| protocol | flow dispatch, TCP/HTTP parsing, HTTP pairing |
| llm      | LLM request classification + call emission  |
| turn     | agent turn assembly (per turn-shard)        |
| metrics  | sliding-window aggregation                  |
| storage  | write buffer + backend flush                |

## Counters

### capture

| Short         | Semantics                                          |
|---------------|----------------------------------------------------|
| `pkts_recv`   | Packets received from the capture source           |
| `pkts_drop`   | Packets dropped by libpcap (kernel buffer overflow) |
| `batches_recv`| cloud-probe batches received                       |
| `batches_drop`| cloud-probe batches dropped (invalid / backpressure)|
| `hb_emit`     | Heartbeats emitted from this source                |
| `read_errors` | Transient read failures (`pcap_next_packet` / ZMQ `recv`) — non-fatal, source keeps running |
| `dump_errors` | Errors writing the optional pcap dump              |

### protocol

| Short             | Semantics                                               |
|-------------------|---------------------------------------------------------|
| `dispatched`      | Packets routed from dispatcher into a flow worker       |
| `hb_drop`         | Heartbeats dropped at the dispatcher (worker full)      |
| `net_parsed`      | L2–L4 packets parsed by the flow worker                 |
| `http_req`        | HTTP requests parsed                                    |
| `http_resp`       | HTTP responses parsed                                   |
| `sse_events`      | SSE events parsed                                       |
| `http_resync`     | TCP stream resync events (recovery from partial HTTP parse) |
| `flows_expired`   | TCP flows garbage-collected for idle timeout            |
| `http_done`       | HTTP exchanges paired successfully (req + resp)         |
| `http_incomplete` | Responses arriving without a matching pending request (pairing failed) |
| `http_expired`    | Pending pairings aged out before a response arrived     |

### llm

| Short            | Semantics                                                                    |
|------------------|------------------------------------------------------------------------------|
| `http_detected`  | HTTP requests classified as an LLM API call (matched by wire-api registry)   |
| `http_ignored`   | HTTP requests not matching any LLM wire-api                                  |
| `calls_completed`| LLM calls emitted downstream (Complete events)                               |
| `calls_agent`    | Subset of `calls_completed` that were attached to an agent session (profile match) |
| `calls_no_agent` | Subset of `calls_completed` with no agent attached (unassigned)              |

### turn

Per turn-shard. Finalization path: `calls_ingested` → buffered until the profile's terminal predicate fires (or grace/idle timer), then `completed` plus exactly one of `fin_grace` / `fin_idle`.

| Short           | Semantics                                                                                     | Rising values usually mean                                                         |
|-----------------|-----------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------|
| `calls_ingested`| Calls accepted into a SessionBuffer                                                           | Should track `llm::calls_agent`                                                    |
| `calls_aux`     | Auxiliary one-shots skipped (e.g. claude-cli session-title)                                   | Steady-state baseline; no action needed                                            |
| `calls_late`    | Call dropped at buffer entry guard (`request_time` < high-water of already-finalized session) | Severe fan-in jitter, broken sharding (same session crossing shards), replay skew  |
| `completed`     | Turns finalized and emitted downstream                                                        | Healthy throughput                                                                 |
| `fin_grace`     | Subset of `completed` closed via grace expiry (terminal observed)                             | Normal path; expect this to dominate                                               |
| `fin_idle`      | Subset of `completed` closed via idle timeout (no terminal observed)                          | Truncated capture, missing terminal predicate, or client crash                     |
| `no_user_start` | Partition discarded because no call carried `is_user_turn_start = Some(true)`                 | Lost capture window at session boundary; orphan sub-agent traffic; profile mis-classifying user-start |

**Tuning `grace_ms`** (config: `[pipeline.turn] grace_ms`, default 1000): the grace window is the only added per-call latency in the steady-state path. If `calls_late` rises in correlation with multi-connection sessions, raise `grace_ms`. If turn-emit latency is too high, lower it — but verify that `no_user_start` and `calls_late` don't climb in response.

### metrics

| Short           | Semantics                                              |
|-----------------|--------------------------------------------------------|
| `events_recv`   | CallStart + CallEnd events received by the aggregator  |
| `windows_flush` | Sliding windows flushed to the storage channel         |

### storage

| Short          | Semantics                                         |
|----------------|---------------------------------------------------|
| `buffered`     | Rows added to the write buffer                    |
| `flushed`      | Rows flushed to the backend                       |
| `flush_errors` | Backend flush errors                              |

## Gauges (queue depths)

Gauges print as the current value. Convention: `q.in` = input queue of the stage, `q.out` = output queue.

| Group    | Short         | Probes                                           |
|----------|---------------|--------------------------------------------------|
| protocol | `q.in`        | Capture → dispatcher (raw packet channel)        |
| protocol | `q.out`       | Dispatcher → flow-worker (routed packets)        |
| llm      | `q.out`       | Flow-worker → HTTP joiner (protocol events)      |
| turn     | `q.in`        | LLM → turn-shard (per-shard channel, summed)     |
| metrics  | `q.in`        | LLM → metrics-shard (per-shard channel, summed)  |
| storage  | `q.calls`     | LlmCall queue into sink                          |
| storage  | `q.turns`     | AgentTurn queue into sink                        |
| storage  | `q.metrics`   | LlmMetric queue into sink                        |
| storage  | `q.exchanges` | HttpExchange queue into sink                     |

Rising storage queues mean the backend cannot keep up — flip to a faster backend or widen the write batch.

## Sample output

```
[INTERNAL] pipeline.remote | capture  | pkts_recv=765190/62 pkts_drop=0/0 batches_recv=26982/2 batches_drop=0/0 hb_emit=39049/3 read_errors=0/0 dump_errors=0/0
[INTERNAL] pipeline.remote | protocol | dispatched=765190/62 hb_drop=0/0 net_parsed=765190/62 http_req=22276/2 http_resp=21956/2 sse_events=319818/25 http_resync=13/0 flows_expired=12123/0 http_done=21956/2 http_incomplete=0/0 http_expired=314/0 q.in=0 q.out=0
[INTERNAL] pipeline.remote | llm      | http_detected=10890/2 http_ignored=11386/0 calls_completed=10574/0 calls_agent=0/0 calls_no_agent=10574/0 q.out=0
[INTERNAL] pipeline.remote | turn     | calls_ingested=0/0 calls_aux=0/0 calls_late=0/0 completed=0/0 fin_grace=0/0 fin_idle=0/0 no_user_start=0/0 q.in=0
[INTERNAL] pipeline.remote | metrics  | events_recv=333856/26 windows_flush=45108/20 q.in=0
[INTERNAL] pipeline.remote | storage  | buffered=... flushed=... flush_errors=... q.calls=0 q.turns=0 q.metrics=0 q.exchanges=0
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
ts-common/src/internal_metrics.rs   # Metric enum + registry + reporter
```
