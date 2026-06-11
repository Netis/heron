# Capture Module Design

## Overview

The capture module acquires raw network packets from four source types and outputs `RawPacket` streams. Each source feeds its own independent processing pipeline; pipelines converge at the storage layer. Three sources are packet taps (live NIC, pcap file, cloud-probe ZMQ); the fourth — [eBPF SSL uprobes](#ebpf-ssl-uprobe-capture-linux-experimental) — is Linux-only and experimental, and reads plaintext at the in-process TLS boundary rather than off the wire.

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

## eBPF SSL-uprobe capture (Linux, experimental)

The three sources above are packet taps: they see whatever is on the wire, which
for TLS traffic is ciphertext — so Heron must be placed where the bytes are
already decrypted (post-terminator, or on a host doing plaintext HTTP). The eBPF
source removes that constraint **on the host itself**: it attaches uprobes to the
TLS library's `SSL_read` / `SSL_write` and reads the plaintext buffers the
application hands to (or gets back from) the library — i.e. *before* encryption
on write and *after* decryption on read. No proxy, no terminator, no MITM, and
nothing on the request path.

It also yields something the packet taps cannot: **process attribution**. The
uprobe fires in the context of the calling thread, so each captured exchange
carries the owning process's `pid`, `comm`, and resolved executable path
(`/proc/<pid>/exe`) — answering *which agent process made this call*, not just
*which 5-tuple*.

### Feeding the existing pipeline by frame synthesis

The eBPF source does **not** open a new entry point into the protocol/LLM
stack. Plaintext chunks from `SSL_read` / `SSL_write` are dressed as synthetic
Ethernet + IPv4 + TCP frames (`FlowSynthesizer`) and emitted as ordinary
`RawPacket`s, so the unchanged dispatcher → TCP reassembler → HTTP/SSE parser →
wire-API decoder → turn tracker runs exactly as it does for a real tap. Key
invariants the synthesizer maintains:

- One synthetic bidirectional flow per `(pid, SSL*)` connection; `SSL_write` ⇒
  client→server, `SSL_read` ⇒ server→client, with monotonically advancing
  per-direction sequence numbers.
- IP `total_length` covers exactly the carried payload; no checksums are
  computed (the L3/L4 decoders don't validate them). A large `SSL_write` is
  split across multiple segments to stay under the IPv4 length limit.
- The connection's first event synthesizes a SYN/SYN-ACK and teardown
  (`SSL_shutdown` / close / process exit) synthesizes a FIN, so turns finalize
  promptly instead of waiting for the flow-timeout sweep. A mid-stream attach
  with no handshake re-syncs on the first observed payload.
- The kernel boot-clock timestamp (`bpf_ktime_get_ns`) is mapped to Unix epoch
  via a one-time `CLOCK_REALTIME − CLOCK_MONOTONIC` offset.

`RawPacket` carries an optional `process: Option<ProcessInfo>` that the
synthesizer stamps from the uprobe event; it threads through `ParsedPacket` →
`TcpFlow` → the HTTP request/response data → `LlmCall`, and lands in the
`process_pid` / `process_comm` / `process_exe` storage columns. All non-eBPF
sources leave it `None`.

### Target coverage

- **Dynamically-linked OpenSSL / BoringSSL** — attached by exported symbol
  (`SSL_read` / `SSL_write` / `SSL_shutdown`) on the discovered `libssl.so`.
  Covers Python SDKs (httpx), curl, and most CLIs.
- **Statically-linked, symbol-stripped BoringSSL** — e.g. Claude Code's Bun
  runtime, which embeds BoringSSL and strips every `SSL_*` symbol. Located by
  **byte-signature → ELF file offset → offset uprobe**; a built-in `flavor =
  "bun"` ships read-anchored prologue signatures so it works with zero manual
  derivation. See [eBPF capture for static-binary TLS](03-ebpf-static-targets.md).

### Limitations & requirements

- **Linux only**, and built behind the non-default `ebpf` cargo feature on
  `h-capture` (prebuilt release binaries do not include it — build from source).
  Requires `CAP_BPF` + `CAP_PERFMON` (kernel ≥ 5.8) or root, plus kernel BTF
  (`/sys/kernel/btf/vmlinux`). `heron doctor` reports the `capture.ebpf` check.
- **HTTP/1.x only** — like every Heron source, the parser does not reconstruct
  HTTP/2; a client that negotiates h2 over ALPN decrypts to HPACK/binary frames
  that are dropped. (Bun's `fetch` offers only HTTP/1.1, so Claude Code is
  covered; a generic `curl` may negotiate h2.)
- **Synthetic 5-tuple today** — the real socket 5-tuple recovery (connect
  kprobe) is a follow-up; the current source uses a deterministic synthetic
  tuple plus mid-stream sync, which the wire-API identification (method / URI /
  Host, not IP) is unaffected by.

Config lives under a `type = "ebpf"` source — see
[Configure → eBPF source](../configure.md#ebpf--on-host-tls-capture-linux-experimental).

## Output

The capture module outputs raw packet bytes without any link-layer processing:

```rust
pub struct RawPacket {
    pub timestamp: Timestamp,   // Microsecond precision
    pub caplen: u32,            // Captured length
    pub wirelen: u32,           // Original wire length
    pub data: Bytes,            // Raw packet bytes as captured
    pub process: Option<ProcessInfo>, // Owning process — eBPF source only; None for packet taps
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
