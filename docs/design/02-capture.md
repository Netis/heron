# Capture Module Design

## Overview

The capture module acquires raw network packets from three source types and outputs `RawPacket` streams. Each source feeds its own independent processing pipeline; pipelines converge at the storage layer.

## Data Sources

### 1. Pcap — Local NIC Capture

Captures packets directly on the host via libpcap. Deployed on the inference server or gateway, capturing plaintext HTTP traffic after TLS termination.

- Rust crate: `pcap`
- Runs on a dedicated OS thread (`std::thread`), because `pcap::Capture::next_packet()` is blocking and must not run inside the Tokio runtime
- BPF filter applied at kernel level to capture only relevant traffic (e.g. `tcp port 8080`)
- Outputs raw packet bytes as captured (including Ethernet header)

### 2. Pcap File — Offline Replay

Reads packets from a `.pcap` / `.pcapng` file. Used for testing, debugging, and replaying captured traffic.

- Rust crate: `pcap` (same as live capture, using `Capture::from_file()`)
- Runs on a dedicated OS thread (`std::thread`), same as live pcap
- Reads at original packet timestamps (preserving inter-packet timing) or as fast as possible (configurable)
- BPF filter can be applied post-read to filter relevant traffic
- Source ends when file is fully read; pipeline drains and completes

### 3. Cloud-Probe — Remote Packet Ingestion via ZMQ

Receives batched packets from [cloud-probe](https://github.com/Netis/cloud-probe) instances deployed on remote servers.

- Rust crate: `zeromq` ([zmq.rs](https://github.com/zeromq/zmq.rs)) — pure Rust ZMQ implementation, native Tokio async, no libzmq dependency
- Uses ZMQ `PULL` socket bound to the configured endpoint (cloud-probe sends via `PUSH` and connects to us)
- `recv_hwm` is a configurable knob (default 1000). zmq.rs 0.4 does not currently expose an RCVHWM option, so the value is accepted and logged but not applied at the socket level. Backpressure flows through the downstream mpsc channel; cloud-probe's own drop statistics cover sender-side loss.
- Runs as a Tokio task (native async, no spawn_blocking needed)
- Extracts individual packets from the ZMQ batch format, outputs raw `pkt_data` bytes as-is (including Ethernet + VLAN + MPLS headers)
- The `version` field in the batch header is **not** validated; any parseable batch is accepted. A malformed batch (length arithmetic inconsistent) is dropped whole and counted in the `CaptureBatchesDropped` internal metric.
- Batch-level metadata (`uuid`, `service_tag`, `keybit`) is currently discarded. If per-probe attribution becomes a requirement, add fields to `RawPacket` and plumb downstream.

#### Cloud-Probe ZMQ Wire Format

All multi-byte fields are **network byte order (big-endian)**.

```
ZMQ Message (one batch per zmq_send):

┌──────────────── Batch Header (24 bytes) ────────────────┐
│ version: u16 │ pkts_num: u16 │ keybit: u32 │ uuid: [u8; 16] │
└─────────────────────────────────────────────────────────┘

Repeated pkts_num times:
┌──────────────── Per-Packet ─────────────────────────────┐
│ pkt_data_len: u16  (total length including MPLS header) │
│ pkt_hdr (16 bytes):                                     │
│   tv_sec: u32, tv_usec: u32, caplen: u32, len: u32     │
│ pkt_data: [u8; pkt_data_len]                            │
│   = Ethernet + [VLAN] + MPLS(4B) + IP payload           │
└─────────────────────────────────────────────────────────┘
```

**MPLS Header (4 bytes):** Injected by cloud-probe into the Ethernet frame with ether_type `0x8847`. Stripping happens in `ts-protocol`'s L2 decoder, which unwinds the label stack until it finds the bottom-of-stack bit and then peeks the next nibble to detect IPv4 vs IPv6.

## Output

The capture module outputs raw packet bytes without any link-layer processing:

```rust
pub struct RawPacket {
    pub timestamp: Timestamp,   // Microsecond precision
    pub caplen: u32,            // Captured length
    pub wirelen: u32,           // Original wire length
    pub data: Bytes,            // Raw packet bytes as captured
}
```

Link-layer stripping (Ethernet, VLAN, MPLS) is the responsibility of the downstream `ts-protocol` crate.

## Runtime Model

Each source runs independently with its own downstream pipeline. Pipelines converge at the storage layer.

```
Source A (pcap eth0)         → channel → pipeline A (protocol → llm → storage)──┐
Source B (pcap file)         → channel → pipeline B (protocol → llm → storage)──┤
Source C (cloud-probe :5555) → channel → pipeline C (protocol → llm → storage)──┼──▶ DB
Source D (cloud-probe :5556) → channel → pipeline D (protocol → llm → storage)──┘
```

- Pcap source: dedicated OS thread → bounded mpsc channel
- Cloud-probe source: Tokio task → bounded mpsc channel
- Backpressure: when a channel is full, pcap blocks on send (libpcap kernel buffer absorbs bursts); cloud-probe ZMQ HWM drops at the sender side. Neither case causes OOM.

## Configuration

```toml
[[capture.sources]]
type = "pcap"
interface = "eth0"
bpf_filter = "tcp port 8080"
snaplen = 65535

[[capture.sources]]
type = "pcap-file"
path = "/data/captures/llm-traffic.pcap"
realtime = false          # false = read as fast as possible; true = preserve original timing

[[capture.sources]]
type = "cloud-probe"
endpoint = "tcp://0.0.0.0:5555"
recv_hwm = 1000
```

## File Structure

```
ts-capture/
├── Cargo.toml
└── src/
    ├── lib.rs              # Public API: start_capture() → Receiver<RawPacket>
    ├── pcap.rs             # PcapCapture — live NIC capture
    ├── pcap_file.rs        # PcapFileReader — offline pcap file replay
    ├── cloud_probe.rs      # CloudProbeReceiver + ZMQ batch parsing
    └── packet.rs           # RawPacket, Timestamp types
```
