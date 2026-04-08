# Capture Module Design

## Overview

The capture module acquires raw network packets from two source types and outputs `RawPacket` streams. Each source feeds its own independent processing pipeline; pipelines converge at the storage layer.

## Data Sources

### 1. Pcap — Local NIC Capture

Captures packets directly on the host via libpcap. Deployed on the inference server or gateway, capturing plaintext HTTP traffic after TLS termination.

- Rust crate: `pcap`
- Runs on a dedicated OS thread (`std::thread`), because `pcap::Capture::next_packet()` is blocking and must not run inside the Tokio runtime
- BPF filter applied at kernel level to capture only relevant traffic (e.g. `tcp port 8080`)
- Outputs raw packet bytes as captured (including Ethernet header)

### 2. Cloud-Probe — Remote Packet Ingestion via ZMQ

Receives batched packets from [cloud-probe](https://github.com/Netis/cloud-probe) instances deployed on remote servers.

- Rust crate: `zeromq` ([zmq.rs](https://github.com/zeromq/zmq.rs)) — pure Rust ZMQ implementation, native Tokio async, no libzmq dependency
- Uses ZMQ `PULL` socket (cloud-probe sends via `PUSH`)
- Runs as a Tokio task (native async, no spawn_blocking needed)
- Extracts individual packets from ZMQ batch format, outputs raw `pkt_data` bytes as-is (including Ethernet + MPLS headers)

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

**MPLS Header (4 bytes):** Injected by cloud-probe into the Ethernet frame. Contains `rra` (direction) and `service_tag` fields. Link-layer and MPLS stripping is handled by the downstream protocol layer, not by the capture module.

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
Source B (cloud-probe :5555) → channel → pipeline B (protocol → llm → storage)──┼──▶ DB
Source C (cloud-probe :5556) → channel → pipeline C (protocol → llm → storage)──┘
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
type = "cloud-probe"
endpoint = "tcp://0.0.0.0:5555"
```

## File Structure

```
ts-capture/
├── Cargo.toml
└── src/
    ├── lib.rs              # Public API: start_capture() → Receiver<RawPacket>
    ├── pcap.rs             # PcapCapture implementation
    ├── cloud_probe.rs      # CloudProbeReceiver + ZMQ batch parsing
    └── packet.rs           # RawPacket, Timestamp types
```
