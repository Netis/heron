# ts-protocol Decoder Refactor Design

Date: 2026-04-10

## Context

The `ts-protocol` module currently relies on `RawPacket::ip_offset()` (defined in `ts-capture`) for link-layer stripping, and uses manual byte indexing in `net.rs` for IP/TCP parsing. The [code review](../../review/ts-protocol-review.md) identified several issues:

- IPv4/TCP header length validation is insufficient
- Link-layer stripping logic is split across `ts-capture` and `ts-protocol` with unclear boundaries
- Manual byte indexing is fragile for malformed packets
- Tests lack edge-case coverage for malformed headers

This refactor introduces a composable decoder submodule inside `ts-protocol`, inspired by the `rpank-ende` decoder architecture from rpktminer. It eliminates the `ip_offset()` dependency and centralizes all L2-L4 decoding in one place.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Location | `de/` submodule inside `ts-protocol` | No need for a separate crate; ts-protocol already owns "raw bytes → protocol events" |
| Buffer | Concrete `PacketBuf<'a>` struct | TokenScope packets are always contiguous; trait indirection is premature |
| Headers | `bytemuck::Pod` structs | Zero-copy, compile-time layout guarantees, eliminates offset bugs |
| Decoders | Free functions + dispatchers | Composable pattern without trait ceremony for a single implementation |
| Output | `ParsedPacket` (unchanged) | Downstream (TcpFlow, FlowWorker) already consumes this type |
| Errors | `DecodeError` enum, non-fatal | Caller skips the packet on error |

## Protocol Scope

- **Link types**: Ethernet, Raw IP (101), BSD Loopback/NULL (0), Linux Cooked SLL (113), SLL2 (276)
- **L2.5**: VLAN (802.1Q), QinQ (802.1ad) — recursive stripping
- **L3**: IPv4, IPv6
- **L4**: TCP only

Adding new protocols later = add a decode function + a match arm in the dispatcher.

## Module Structure

```
server/ts-protocol/src/
├── de/                      # New decoder submodule
│   ├── mod.rs               # Public API: decode() entry point, re-exports
│   ├── buf.rs               # PacketBuf<'a> — cursor over raw bytes
│   ├── error.rs             # DecodeError enum + DecodeResult type
│   ├── headers.rs           # Pod header structs (Ethernet, IPv4, IPv6, TCP, VLAN, SLL)
│   ├── l2.rs                # Link-layer decoders + decode_l2()
│   ├── l3.rs                # Network-layer decoders + dispatch_l3()
│   └── l4.rs                # Transport-layer decoders (TCP)
├── net.rs                   # Slimmed: FlowKey, Direction, ParsedPacket, TCP flags
├── tcp.rs                   # Unchanged
├── http.rs                  # Unchanged
├── flow.rs                  # Updated: calls de::decode() instead of net::parse_packet()
├── pipeline.rs              # Unchanged
├── model.rs                 # Unchanged
└── lib.rs                   # Add `mod de;`
```

## Component Details

### `PacketBuf<'a>` (buf.rs)

A cursor over `&[u8]` providing safe, typed access:

```rust
pub struct PacketBuf<'a> {
    data: &'a [u8],
    offset: usize,
    len: usize,
}
```

Key methods:

- `new(data: &'a [u8]) -> Self` — initialize with full slice
- `remaining() -> usize` — bytes left (`len - offset`)
- `get<H: Pod>() -> Option<&'a H>` — zero-copy cast at current offset via `bytemuck::from_bytes()`
- `consume<H: Pod>() -> Option<&'a H>` — `get()` then `advance(size_of::<H>())`; returns `None` if insufficient bytes
- `advance(n: usize)` — move offset forward
- `remaining_slice() -> &'a [u8]` — `data[offset..len]`
- `set_len(len: usize)` — truncate to logical length (e.g., IPv4 `total_length`)

### `DecodeError` (error.rs)

```rust
pub enum DecodeError {
    Truncated,       // Not enough bytes for expected header
    NotSupported,    // Protocol we don't handle (UDP, GRE, etc.)
    NotIp,           // Link layer resolved to non-IP traffic
    InvalidHeader,   // Header field fails validation (IHL < 5, data_offset < 5)
}
```

Non-fatal: `decode()` returns `Option<ParsedPacket>`, converting any `DecodeError` to `None`.

### Macros

```rust
/// Consume a Pod header or return Truncated
macro_rules! try_consume {
    ($buf:expr, $H:ty) => {
        $buf.consume::<$H>().ok_or(DecodeError::Truncated)?
    };
}

/// Skip n bytes or return Truncated
macro_rules! try_skip {
    ($buf:expr, $n:expr) => {
        if $buf.remaining() >= $n {
            $buf.advance($n);
        } else {
            return Err(DecodeError::Truncated);
        }
    };
}
```

### Header Structs (headers.rs)

All `#[repr(C)]` + `Pod` + `Zeroable` with accessor methods for network byte order fields:

| Struct | Size | Accessors |
|--------|------|-----------|
| `EthernetHeader` | 14B | `ether_type() -> u16` |
| `VlanHeader` | 4B | `ether_type() -> u16` |
| `LinuxSllHeader` | 16B | `protocol() -> u16` |
| `LinuxSll2Header` | 20B | `protocol() -> u16` |
| `Ipv4Header` | 20B | `ihl()`, `total_length()`, `protocol()`, `src_ip()`, `dst_ip()` |
| `Ipv6Header` | 40B | `payload_length()`, `next_header()`, `src_ip()`, `dst_ip()` |
| `TcpHeader` | 20B | `src_port()`, `dst_port()`, `seq()`, `ack()`, `data_offset()`, `flags()` |

Fields stored as `[u8; N]` with `from_be_bytes()` accessors — avoids needing a `NetEndian<T>` wrapper.

### Decoder Functions

**L2 — `l2.rs`**:

- `decode_l2(buf, link_type) -> DecodeResult<u16>` — dispatch by pcap link type, return EtherType
- `decode_ethernet(buf) -> DecodeResult<u16>` — consume Ethernet header, delegate to `strip_vlan()`
- `strip_vlan(buf, ether_type) -> DecodeResult<u16>` — recursive VLAN/QinQ stripping
- `decode_null(buf) -> DecodeResult<u16>` — BSD loopback (4-byte AF header)
- `detect_raw_ip(buf) -> DecodeResult<u16>` — peek first nibble for v4/v6
- `decode_linux_sll(buf) -> DecodeResult<u16>` — SLL v1
- `decode_linux_sll2(buf) -> DecodeResult<u16>` — SLL v2

**L3 — `l3.rs`**:

- `dispatch_l3(buf, ether_type) -> DecodeResult<L3Info>` — route by EtherType
- `decode_ipv4(buf) -> DecodeResult<L3Info>` — parse IPv4, validate IHL >= 20, skip options, truncate to total_length
- `decode_ipv6(buf) -> DecodeResult<L3Info>` — parse IPv6 (extension headers deferred)

```rust
pub struct L3Info {
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub protocol: u8,
}
```

**L4 — `l4.rs`**:

- `dispatch_l4(buf, protocol) -> DecodeResult<L4Info>` — route by IP protocol
- `decode_tcp(buf) -> DecodeResult<L4Info>` — parse TCP, validate data_offset >= 20, skip options

```rust
pub struct L4Info {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
}
```

### Entry Point (mod.rs)

```rust
pub fn decode(data: &[u8], link_type: u32, timestamp_us: i64) -> Option<ParsedPacket> {
    let mut buf = PacketBuf::new(data);

    let ether_type = decode_l2(&mut buf, link_type).ok()?;
    let l3 = dispatch_l3(&mut buf, ether_type).ok()?;
    let l4 = dispatch_l4(&mut buf, l3.protocol).ok()?;

    let payload = Bytes::copy_from_slice(buf.remaining_slice());
    let flow_key = FlowKey::new(l3.src_ip, l4.src_port, l3.dst_ip, l4.dst_port);
    let direction = flow_key.direction(l3.src_ip, l4.src_port);

    Some(ParsedPacket {
        flow_key,
        direction,
        src_ip: l3.src_ip,
        src_port: l4.src_port,
        dst_ip: l3.dst_ip,
        dst_port: l4.dst_port,
        tcp_flags: l4.flags,
        tcp_seq: l4.seq,
        tcp_ack: l4.ack,
        payload,
        timestamp_us,
    })
}
```

## Migration Plan

### What Changes

1. **`ts-protocol/Cargo.toml`**: Add `bytemuck = { version = "1", features = ["derive"] }`
2. **`ts-protocol/src/lib.rs`**: Add `mod de;`
3. **`ts-protocol/src/de/`**: New submodule (7 files)
4. **`ts-protocol/src/net.rs`**: Remove `parse_packet()`, `parse_ipv4_tcp()`, `parse_ipv6_tcp()`, `parse_tcp()`. Keep `FlowKey`, `Direction`, `ParsedPacket`, TCP flag constants.
5. **`ts-protocol/src/flow.rs`**: Call `de::decode()` instead of `net::parse_packet()`
6. **`ts-capture/src/packet.rs`**: Remove `ip_offset()`, `ethernet_ip_offset()`, all `LINKTYPE_*`/`ETHERTYPE_*` constants, VLAN/QinQ stripping logic. `RawPacket` becomes a pure data envelope.

### What Stays Unchanged

- `tcp.rs`, `http.rs`, `pipeline.rs`, `model.rs` — consume `ParsedPacket`, unaffected
- `FlowWorker`, `TcpFlow`, `HttpParser` — no interface changes
- Downstream crates (`ts-llm`, `ts-metrics`, etc.) — consume `ProtocolEvent`, unaffected

### Boundary After Refactor

- **`ts-capture`**: Produces `RawPacket { timestamp_us, caplen, wirelen, link_type, data }` — pure data, no parsing
- **`ts-protocol::de`**: Decodes raw bytes → `ParsedPacket` — owns all L2-L4 parsing
- **`ts-protocol::tcp/http`**: Consumes `ParsedPacket` → TCP reassembly → HTTP parsing → `ProtocolEvent`

## Testing Strategy

### Unit Tests (per layer)

**L2**:
- Ethernet → correct EtherType
- Single VLAN tag → strips, returns inner EtherType
- QinQ → strips both tags
- Linux SLL / SLL2 → correct protocol extraction
- BSD loopback → AF_INET / AF_INET6
- Raw IP → first nibble detection
- Truncated Ethernet (13 bytes) → `Truncated` error

**L3**:
- IPv4 standard 20-byte header → correct IPs, protocol
- IPv4 with options (IHL > 5) → options skipped
- IPv4 invalid IHL (< 5) → `InvalidHeader` error
- IPv4 total_length → buf truncated correctly
- IPv6 standard 40-byte header → correct IPs, next_header
- Non-IP EtherType (ARP) → `NotIp` error

**L4**:
- TCP standard 20-byte header → correct ports, seq, ack, flags
- TCP with options (data_offset > 5) → options skipped
- TCP invalid data_offset (< 5) → `InvalidHeader` error
- Non-TCP protocol (UDP) → `NotSupported` error

### Integration Tests (full decode)

- Ethernet + IPv4 + TCP → correct `ParsedPacket`
- Ethernet + IPv6 + TCP → correct `ParsedPacket`
- VLAN + IPv4 + TCP → correct `ParsedPacket`
- Raw IP + TCP → correct `ParsedPacket`
- Linux SLL + IPv4 + TCP → correct `ParsedPacket`
- Various truncation points → `None` (graceful skip)

### Regression

- All existing `ts-protocol` tests must continue to pass
