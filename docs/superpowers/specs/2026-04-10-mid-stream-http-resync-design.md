# Mid-Stream HTTP Capture Resync Design

## Problem

TokenScope's HTTP parser assumes it observes TCP connections from the start (SYN). When capture begins mid-connection — either because the TokenScope process starts while connections are active, or because ZMQ packet delivery from cloud-probe drops packets — the parser fails silently:

- **Missing request:** Parser stuck in `WaitingForRequest`, server-side data accumulates in buffer unused.
- **Mid-stream data corruption:** Packet loss mid-flow causes body decoding errors. Current code skips bytes and continues, producing garbage.
- **Lost response:** Client sends a new request while parser is still waiting for the previous response. Parser deadlocks on the old round.

## Design

Dual-layer resync: TCP layer filters packets before they enter buffers; HTTP layer detects parsing failures and signals the TCP layer to resync.

### Layer Responsibilities

**TCP layer (TcpFlow):** Controls what enters buffers. Performs per-packet inspection using `looks_like_http_request()` on raw packet payload (O(1) check on first few bytes). Manages `synced` state.

**HTTP layer (HttpParser):** Detects unrecoverable parse errors during req-resp cycle. Reports `NeedResync` to TCP layer. Does not attempt recovery (no byte-skipping).

### TcpFlow State: `synced: bool`

Initial value: `false`.

Transitions:

| From | Trigger | Action | To |
|------|---------|--------|----|
| `false` | SYN packet | Determine `client_side` | `true` |
| `false` | Client-direction packet with `looks_like_http_request(payload)` | Determine `client_side`, clear both buffers, append + parse | `true` |
| `false` | Any other packet | Discard | `false` |
| `true` | Client-direction packet + `HttpParser::is_waiting_for_response()` + `looks_like_http_request(payload)` | Clear both buffers, reset HttpParser, append + parse from this packet | `true` |
| `true` | `HttpParser::parse()` returns `NeedResync` | Clear both buffers, reset HttpParser | `false` |

The `synced` field unifies with `ClientSide` determination: `synced == true` implies `client_side != Unknown`. The existing `ClientSide::Unknown` branch in `try_parse_http()` that checks buffers is removed — replaced by per-packet pre-check in `push()`.

### HttpParser Changes

**New return type:**

```rust
pub enum ParseResult {
    Ok,
    NeedResync,
}
```

`parse()` returns `ParseResult` instead of `()`.

**NeedResync triggers:**

| State | Condition | Current behavior | New behavior |
|-------|-----------|------------------|--------------|
| WaitingForRequest | `httparse::Request::parse` returns `Err` | Skip 1 byte | `NeedResync` |
| ReadingRequestBody | Chunked: invalid chunk size hex | Skip line, continue | `NeedResync` |
| WaitingForResponse | `httparse::Response::parse` returns `Err` | Skip 1 byte | `NeedResync` |
| ReadingResponseBody | Chunked: invalid chunk size hex | Skip line, continue | `NeedResync` |

All byte-skipping recovery logic is removed.

**New public methods:**

```rust
/// Returns true when parser is in WaitingForResponse or ReadingResponseBody.
pub fn is_waiting_for_response(&self) -> bool

/// Reset parser to initial state (WaitingForRequest), clearing all pending data.
pub fn reset(&mut self)
```

### Observability

- **Metric:** `Resync` variant added to `Metric` enum — counter incremented each time `synced` transitions to `false` (or on direct resync in `true → true`).
- **Log:** `trace!` level with flow_key and trigger reason.

### `looks_like_http_request` Visibility

Changed from private to `pub(crate)` in `tcp.rs` so both layers can reference it. No relocation needed.

## What This Does NOT Handle

- **HTTP pipelining:** Not relevant — LLM APIs are strict req-resp pairs.
- **Partial recovery of incomplete rounds:** Incomplete req-resp pairs are discarded entirely. A response without its request has no analysis value (missing URI, model, prompt).
- **Out-of-order TCP segments:** Existing `append_payload` drops out-of-order packets. This design does not change that behavior.

## File Changes

| File | Changes |
|------|---------|
| `ts-protocol/src/http.rs` | Add `ParseResult` enum; `parse()` returns `ParseResult`; replace byte-skip with `NeedResync`; add `is_waiting_for_response()`, `reset()`; remove chunk-skip logic in `read_chunk` |
| `ts-protocol/src/tcp.rs` | Add `synced` field to `TcpFlow`; rework `push()` with synced/unsynced logic; remove `ClientSide::Unknown` buf-level detection in `try_parse_http()`; make `looks_like_http_request` `pub(crate)` |
| `ts-common/src/internal_metrics.rs` | Add `Resync` to `Metric` enum |

## Tests

- **HttpParser:** Each error scenario returns `NeedResync` (corrupt request header, corrupt response header, invalid chunk size in request body, invalid chunk size in response body).
- **TcpFlow mid-stream join:** First packets are server-direction data → discarded → client sends request → synced → normal parsing.
- **TcpFlow mid-flow corruption:** Normal parsing → HttpParser returns NeedResync → unsynced → next request packet → resynced → normal parsing.
- **TcpFlow new request during response wait:** Parsing request, then waiting for response → client packet with new request → resync directly to new request.
