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

| Short              | Semantics                                          |
|--------------------|----------------------------------------------------|
| `pkts_recv`        | Packets received from the capture source           |
| `kern_pkts_drop`   | Packets dropped by libpcap (kernel ring buffer overflow) |
| `batches_recv`     | cloud-probe batches received                       |
| `zmq_batches_drop` | cloud-probe batches dropped (invalid / backpressure) |
| `heartbeats`       | Heartbeats emitted from this source                |
| `read_errors`      | Transient read failures (`pcap_next_packet` / ZMQ `recv`) — non-fatal, source keeps running |
| `dump_errors`      | Errors writing the optional pcap dump              |

### protocol

| Short                | Semantics                                               |
|----------------------|---------------------------------------------------------|
| `pkts_routed`        | Packets routed from dispatcher into a flow worker       |
| `heartbeats_drop`    | Heartbeats dropped at the dispatcher (worker full)      |
| `net_parsed`         | L2–L4 packets parsed by the flow worker                 |
| `parse_drop_notip`   | Packets dropped at net-parse: not IP / not supported     |
| `parse_drop_nottcp`  | Packets dropped at net-parse: IP but not TCP            |
| `parse_drop_bad`     | Packets dropped at net-parse: truncated / invalid header |
| `http_req`           | HTTP requests parsed                                    |
| `http_resp`          | HTTP responses parsed                                   |
| `sse_events`         | SSE events parsed                                       |
| `http_resync`        | TCP stream resync events (recovery from partial HTTP parse) |
| `flows_expired`      | TCP flows garbage-collected for idle timeout            |
| `http_done`          | HTTP exchanges paired successfully (req + resp)         |
| `http_unpaired`      | Responses arriving without a matching pending request (pairing failed) |
| `http_expired`       | Pending pairings aged out before a response arrived     |

### llm

| Short            | Semantics                                                                          |
|------------------|------------------------------------------------------------------------------------|
| `http_detected`  | HTTP requests classified as an LLM API call (matched by wire-api registry)         |
| `http_ignored`   | HTTP requests not matching any LLM wire-api                                        |
| `calls_agent`    | LLM calls emitted downstream that were attached to an agent session (profile match) |
| `calls_no_agent` | LLM calls emitted downstream with no agent attached (unassigned)                   |

### turn

Per turn-shard. Finalization path: `calls_ingested` → buffered until the profile's terminal predicate fires (or grace/idle timer), then `completed` plus exactly one of `closed_grace` / `closed_idle`.

| Short           | Semantics                                                                                     | Rising values usually mean                                                         |
|-----------------|-----------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------|
| `calls_ingested`| Calls accepted into a SessionBuffer                                                           | Should track `llm::calls_agent`                                                    |
| `calls_aux`     | Auxiliary one-shots skipped (e.g. claude-cli session-title)                                   | Steady-state baseline; no action needed                                            |
| `calls_late`    | Call dropped at buffer entry guard (`request_time` < high-water of already-finalized session) | Severe fan-in jitter, broken sharding (same session crossing shards), replay skew  |
| `completed`     | Turns finalized and emitted downstream                                                        | Healthy throughput                                                                 |
| `closed_grace`  | Subset of `completed` closed via grace expiry (terminal observed)                             | Normal path; expect this to dominate                                               |
| `closed_idle`   | Subset of `completed` closed via idle timeout (no terminal observed)                          | Truncated capture, missing terminal predicate, or client crash                     |
| `no_user_start` | Partition discarded because no call carried `is_user_turn_start = Some(true)`                 | Lost capture window at session boundary; orphan sub-agent traffic; profile mis-classifying user-start |

**Tuning `grace_ms`** (config: `[pipeline.turn] grace_ms`, default 1000): the grace window is the only added per-call latency in the steady-state path. If `calls_late` rises in correlation with multi-connection sessions, raise `grace_ms`. If turn-emit latency is too high, lower it — but verify that `no_user_start` and `calls_late` don't climb in response.

### metrics

| Short              | Semantics                                              |
|--------------------|--------------------------------------------------------|
| `llm_events_recv`  | CallStart + CallEnd events received by the aggregator  |
| `windows_flush`    | Sliding windows flushed to the storage channel         |

### storage

| Short          | Semantics                                         |
|----------------|---------------------------------------------------|
| `buffered`     | Rows added to the write buffer                    |
| `flushed`      | Rows flushed to the backend                       |
| `flush_errors` | Backend flush errors                              |

## Gauges (queue depths)

Gauges print the current depth (max across shards for sharded queues). Every queue is named after the content it carries (`RawPacket` → `q.raw`, `HttpParseEvent` → `q.http_parse_evt`, `HttpJoinerEvent` → `q.http_joiner_evt`, `LlmEvent` → `q.llm_evt`, `AgentCall` → `q.agent_call`, etc.), so the name identifies the queue regardless of which side of a stage you stand on.

| Group    | Short               | Carries             | Probes                                                |
|----------|---------------------|---------------------|-------------------------------------------------------|
| protocol | `q.raw`             | `RawPacket`         | Capture → dispatcher                                  |
| protocol | `q.parsed`          | `WorkerInput`       | Dispatcher → flow-worker (net-parsed packets)         |
| llm      | `q.http_parse_evt`  | `HttpParseEvent`    | Flow-worker (HTTP parse) → HTTP joiner                |
| llm      | `q.http_joiner_evt` | `HttpJoinerEvent`   | HTTP joiner → LLM stage                               |
| turn     | `q.agent_call`      | `TurnShardInput`    | LLM → turn-shard (`AgentCall` + heartbeats)           |
| metrics  | `q.llm_evt`         | `LlmEvent`          | LLM → metrics-shard                                   |
| storage  | `q.calls`      | `Arc<LlmCall>`      | LlmCall queue into sink                          |
| storage  | `q.turns`      | `AgentTurn`         | AgentTurn queue into sink                        |
| storage  | `q.metrics`    | `LlmMetric`         | LlmMetric queue into sink                        |
| storage  | `q.exchanges`  | `HttpExchange`      | HttpExchange queue into sink                     |

Rising storage queues mean the backend cannot keep up — flip to a faster backend or widen the write batch.

## Sample output

```
[INTERNAL] pipeline.remote | capture  | pkts_recv=765190/62 kern_pkts_drop=0/0 batches_recv=26982/2 zmq_batches_drop=0/0 heartbeats=39049/3 read_errors=0/0 dump_errors=0/0
[INTERNAL] pipeline.remote | protocol | pkts_routed=765190/62 heartbeats_drop=0/0 net_parsed=765190/62 parse_drop_notip=0/0 parse_drop_nottcp=0/0 parse_drop_bad=0/0 http_req=22276/2 http_resp=21956/2 sse_events=319818/25 http_resync=13/0 flows_expired=12123/0 http_done=21956/2 http_unpaired=0/0 http_expired=314/0 q.raw=0 q.parsed=0
[INTERNAL] pipeline.remote | llm      | http_detected=10890/2 http_ignored=11386/0 calls_agent=0/0 calls_no_agent=10574/0 q.http_parse_evt=0 q.http_joiner_evt=0
[INTERNAL] pipeline.remote | turn     | calls_ingested=0/0 calls_aux=0/0 calls_late=0/0 completed=0/0 closed_grace=0/0 closed_idle=0/0 no_user_start=0/0 q.agent_call=0
[INTERNAL] pipeline.remote | metrics  | llm_events_recv=333856/26 windows_flush=45108/20 q.llm_evt=0
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
