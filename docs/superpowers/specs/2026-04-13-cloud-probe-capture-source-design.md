# Cloud-Probe Capture Source Design

Date: 2026-04-13
Status: Approved, pending implementation plan

## Goal

Implement the third capture source type — `cloud-probe` — so TokenScope can receive packets batched over ZMQ from [cloud-probe](https://github.com/Netis/cloud-probe) instances. The `CaptureSourceConfig::CloudProbe` variant already exists in config, but `ts-capture::factory::build_source` currently returns an error for it; this design fills that gap and also adds the MPLS stripping that cloud-probe's wire format requires downstream.

Scope is limited to what's needed to get cloud-probe packets flowing end-to-end through the existing pipeline (capture → protocol → llm → metrics → storage) with the same operational visibility as the pcap sources.

## Cloud-Probe Wire Format (Reference)

All multi-byte fields are network byte order (big-endian). Source of truth: `cloud-probe/cpworker/src/output_zmq.{h,c}`.

```
ZMQ Message (one batch per zmq_send):

┌──────────────── Batch Header (24 bytes) ────────────────┐
│ version: u16 │ pkts_num: u16 │ keybit: u32 │ uuid: [u8; 16] │
└─────────────────────────────────────────────────────────┘

Repeated pkts_num times:
┌──────────────── Per-Packet ─────────────────────────────┐
│ pkt_data_len: u16   (total length including MPLS header) │
│ pkt_hdr (16 bytes):                                     │
│   tv_sec: u32, tv_usec: u32, caplen: u32, len: u32      │
│ pkt_data: [u8; pkt_data_len]                            │
│   = Ethernet + [VLAN] + MPLS(4B) + IP payload           │
└─────────────────────────────────────────────────────────┘
```

Cloud-probe overwrites the Ethernet (or innermost VLAN) ether_type to `0x8847` (MPLS) and injects a 4-byte MPLS shim before the IP header. The shim carries cloud-probe-specific fields (`rra` direction, `service_tag`, `bottom=1`); TokenScope treats it opaquely — we only need to skip past it.

## Design Decisions (Resolved During Brainstorming)

1. **MPLS stripping is a `ts-protocol` L2 decoder extension**, not a `ts-capture` byte-rewrite. Applies uniformly to any future source that carries MPLS.
2. **Batch metadata (`uuid`, `service_tag`, `keybit`) is discarded.** `RawPacket` stays unchanged. If per-probe attribution is needed later, upgrade path is to add fields to `RawPacket`/`ParsedPacket`/storage schema.
3. **ZMQ socket: PULL bind.** TokenScope listens on `endpoint`; cloud-probe instances connect with PUSH. Multiple probes fan in naturally. Matches `cptools/recvzmq` reference.
4. **Malformed batch handling: drop the whole batch.** Once a length field is wrong the rest of the buffer is unreliable. No `version` validation — treat any parseable batch as valid.
5. **Backpressure: ZMQ HWM is configurable (default 1000); downstream `mpsc::Sender` backpressures the recv loop naturally.** Consistent with pcap sources (blocking send), keeps drop decisions at the probe side where drop stats already exist.
6. **ZMQ library: `zeromq` crate (pure Rust, tokio-native).** No libzmq system dependency; our PULL-bind + recv usage is well within what zmq.rs supports reliably.
7. **Code organization: single file `cloud_probe.rs`.** Wire format parsing is a pure function, unit-testable without a socket.

## Components

### `ts-capture/src/cloud_probe.rs` (new)

```rust
pub struct CloudProbeSource {
    endpoint: String,
    recv_hwm: i32,
}

impl CloudProbeSource {
    pub fn new(endpoint: String, recv_hwm: i32) -> Self { ... }
}

#[async_trait]
impl CaptureSource for CloudProbeSource {
    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<RawPacket>,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> { ... }
}
```

Runtime loop (inside `run`):

1. Create `zeromq::PullSocket`, configure `recv_hwm`, then `.bind(&endpoint)`.
2. `tracing::info!("cloud-probe: listening on {endpoint}, hwm={hwm}")`.
3. Loop with `tokio::select!`:
   - `cancel.cancelled()` → break clean.
   - `socket.recv()` → on success, call `parse_batch(&msg)`.
     - `Ok(pkts)`: `CaptureBatchesReceived.inc()`, then for each packet `tx.send(pkt).await` — if send returns `Err`, downstream is closed, break clean; per-packet `CapturePacketsReceived.inc()`.
     - `Err(_)`: `CaptureBatchesDropped.inc()`, rate-limited `tracing::warn!` (e.g. every 5s, showing byte length and failure reason), continue.
4. On exit, log cumulative batch/packet counts (mirroring pcap sources).

### `parse_batch` (pure function, same file)

```rust
pub(crate) fn parse_batch(bytes: &[u8]) -> Result<Vec<RawPacket>, BatchError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum BatchError {
    #[error("batch truncated: needed {needed} bytes, have {have} at offset {offset}")]
    Truncated { needed: usize, have: usize, offset: usize },
}
```

- Reads 24-byte batch header; extracts `pkts_num` only (ignores `version`, `keybit`, `uuid`).
- Loops `pkts_num` times: reads `pkt_data_len` (u16 BE), `pkt_hdr` (4 × u32 BE), `pkt_data`.
- Each packet → `RawPacket { timestamp_us, caplen, wirelen, link_type: LINKTYPE_ETHERNET, data: Bytes::copy_from_slice(pkt_data) }`.
- `timestamp_us = tv_sec as i64 * 1_000_000 + tv_usec as i64` (matches pcap source).
- Any bounds failure → `Err(Truncated)`; caller drops whole batch.

### `ts-capture/src/factory.rs`

Replace the current `CloudProbe` error branch with:

```rust
CaptureSourceConfig::CloudProbe { endpoint, recv_hwm } => Ok(Box::new(
    CloudProbeSource::new(endpoint.clone(), *recv_hwm),
)),
```

Remove the `test_build_cloud_probe_returns_error` test; add `test_build_cloud_probe_source` mirroring the pcap tests.

### `ts-capture/src/lib.rs`

- `mod cloud_probe;`
- `pub use cloud_probe::CloudProbeSource;`
- `CaptureError` gains `#[error("zmq error: {0}")] Zmq(#[from] zeromq::ZmqError)`.

### `ts-protocol/src/de/headers.rs`

```rust
pub const ETHERTYPE_MPLS: u16 = 0x8847;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MplsHeader {
    pub bytes: [u8; 4],
}

impl MplsHeader {
    /// True if this is the bottom-of-stack label.
    pub fn bottom_of_stack(&self) -> bool {
        // S bit is bit 8 of the 32-bit label entry (third byte, bit 0).
        self.bytes[2] & 0x01 != 0
    }
}
```

### `ts-protocol/src/de/l2.rs`

- `strip_vlan` dispatches to `strip_mpls` when the resolved ether_type is `ETHERTYPE_MPLS`.
- `strip_mpls(buf: &mut PacketBuf) -> DecodeResult<u16>`:
  - Loop: consume one `MplsHeader`; if `bottom_of_stack()` is true, break.
  - After the label stack, peek one byte. First nibble `4` → `ETHERTYPE_IPV4`, `6` → `ETHERTYPE_IPV6`, else `DecodeError::NotIp`.

Covers single-label (cloud-probe) and multi-label MPLS stacks uniformly.

## Configuration

`ts-common/src/config.rs`:

```rust
CloudProbe {
    #[serde(default = "default_cloud_probe_endpoint")]
    endpoint: String,
    #[serde(default = "default_cloud_probe_hwm")]
    recv_hwm: i32,
},

fn default_cloud_probe_hwm() -> i32 { 1000 }
```

`server/config/default.toml` example:

```toml
# [[capture.sources]]
# type = "cloud-probe"
# endpoint = "tcp://0.0.0.0:5555"
# recv_hwm = 1000
```

## Metrics

New counter in `ts-common/src/internal_metrics.rs`:

```rust
CaptureBatchesDropped => { kind: Counter, group: Capture, short: "batches_drop" }
```

Existing `CaptureBatchesReceived` and `CapturePacketsReceived` are reused.

## Dependencies

`server/Cargo.toml` workspace dependencies:

```toml
zeromq = { version = "0.4", default-features = false, features = ["tokio-runtime", "tcp-transport"] }
```

Disabling default features avoids pulling in async-std. `ts-capture/Cargo.toml` takes `zeromq.workspace = true`.

## Error Handling

| Failure | Response |
|---|---|
| `bind()` fails (port in use, permission) | Return `Err(CaptureError::Zmq(_))` from `run()` — same treatment as pcap interface-not-found. |
| `recv()` returns error | Log error, break loop, return `Err(CaptureError::Zmq(_))`. |
| `parse_batch` fails | Log warn (rate-limited), `CaptureBatchesDropped.inc()`, continue. |
| `tx.send()` returns `Err` | Downstream closed, break clean, return `Ok(())`. |
| `cancel.cancelled()` | Break clean, return `Ok(())`. |

## Testing

### Unit (`ts-capture/src/cloud_probe.rs`)

- `parse_batch` happy path: 0 packets / 1 packet / N packets.
- Truncated batch header (< 24 bytes).
- Truncated inner `pkt_hdr`.
- `pkt_data_len` exceeding remaining buffer.
- Timestamp conversion correctness (tv_sec/tv_usec → microseconds).

### Unit (`ts-protocol/src/de/l2.rs`)

- Eth + MPLS (bottom=1) + IPv4.
- Eth + MPLS (bottom=1) + IPv6.
- Eth + VLAN + MPLS + IPv4 (cloud-probe's VLAN case).
- Eth + MPLS(bottom=0) + MPLS(bottom=1) + IPv4 (multi-label stack).
- Eth + MPLS(bottom=1) + non-IP → `NotIp`.
- Truncated MPLS shim.

### Integration (`ts-capture/tests/`, optional but recommended)

- Spin up `zeromq::PushSocket` in-test, send a hand-crafted batch with 3 packets; start `CloudProbeSource` bound to an ephemeral port; assert 3 `RawPacket`s arrive with correct `timestamp_us`, `caplen`, `wirelen`, `data`.
- Tests should be `#[tokio::test]` and pick a free port via `TcpListener` bind-then-drop trick.

## Documentation Updates

`docs/design/capture.md` section "Cloud-Probe — Remote Packet Ingestion via ZMQ":

- Note that `version` is not validated (any parseable batch is accepted).
- Document `recv_hwm` configuration knob (default 1000).
- State that MPLS stripping is performed by `ts-protocol`'s L2 decoder, not by this module.

No changes to `architecture.md`, `schema.md`, `llm.md`, `metrics.md`, or `turn.md`.

## Out of Scope

- Per-probe attribution in business storage (`uuid`/`service_tag` plumbing through to `LlmCall`/`LlmTurn`/`LlmMetric`). Deferred until a concrete query requirement appears.
- CURVE/ZAP authentication for the ZMQ socket. Cloud-probe deployments today rely on network-level isolation.
- TLS/IPC transports. `tcp://` is the only mode cloud-probe's `output_zmq` supports.
- Version negotiation or multi-version batch format support. Only format v2 exists and is assumed.
