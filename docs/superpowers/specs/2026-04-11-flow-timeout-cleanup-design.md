# Flow Timeout Cleanup Design

## Problem

`FlowWorker::flows` (`HashMap<FlowKey, TcpFlow>`) only removes flows when `TcpFlow::is_closed()` returns true (FIN from both sides or RST). If a connection terminates without a captured FIN/RST — due to packet loss, mid-stream capture start, or ZMQ delivery gaps — the flow entry remains in the HashMap indefinitely, leaking memory.

## Design

Packet-timestamp-driven periodic cleanup. No system timers, no extra threads.

### TcpFlow Changes

Add `last_pkt_ts: i64` field — updated on every `push()` call with `pkt.timestamp_us`. Initial value: 0.

### FlowWorker Changes

Add `last_cleanup_ts: i64` field — tracks the packet timestamp of the last cleanup sweep. Initial value: 0.

In `process()`, after processing each packet, check:

```
if pkt.timestamp_us - self.last_cleanup_ts > CLEANUP_INTERVAL_US
```

When triggered, iterate `flows` and remove entries where:

```
pkt.timestamp_us - flow.last_pkt_ts > FLOW_TIMEOUT_US
```

Update `last_cleanup_ts = pkt.timestamp_us` after sweep.

Before removing a timed-out flow, call `finish_pending_response()` to flush any in-progress response (same as RST handling).

### Constants

| Constant | Value | Rationale |
|----------|-------|-----------|
| `CLEANUP_INTERVAL_US` | 30_000_000 (30s) | Amortize scan cost; 30s granularity is sufficient |
| `FLOW_TIMEOUT_US` | 120_000_000 (120s) | Two minutes without a packet — connection is dead |

### Observability

- Existing `Metric::FlowsActive` (if present) or a new `Metric::FlowsTimedOut` counter incremented per removed flow.
- `trace!` log: number of flows removed per sweep.

### Public API

Add `pub fn last_pkt_ts(&self) -> i64` to `TcpFlow` so `FlowWorker` can read it without field access.

## File Changes

| File | Changes |
|------|---------|
| `ts-protocol/src/tcp.rs` | Add `last_pkt_ts` to TcpFlow; add `last_cleanup_ts` + constants to FlowWorker; add cleanup logic in `process()` |
| `ts-common/src/internal_metrics.rs` | Add `FlowsTimedOut` to `Metric` enum (if not already present) |

## Tests

- **Timeout cleanup:** Create flow, push packets with advancing timestamps, advance past timeout → flow removed.
- **Active flow not cleaned:** Flow with recent packets survives cleanup sweep.
- **Pending response flushed:** Flow in ReadingResponseBody times out → HttpResponse emitted before removal.
