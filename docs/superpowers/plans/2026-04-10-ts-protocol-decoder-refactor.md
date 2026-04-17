# ts-protocol Decoder Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace monolithic `parse_packet()` and `RawPacket::ip_offset()` with a composable L2→L3→L4 decoder submodule inside `ts-protocol`.

**Architecture:** New `de/` submodule with `PacketBuf` cursor, `bytemuck::Pod` header structs, free-function decoders, and dispatcher functions. Entry point `de::decode()` returns `ParsedPacket`. Downstream (TcpFlow, HttpParser, pipeline) unchanged.

**Tech Stack:** Rust, bytemuck (new dep), bytes, std::net

**Spec:** `docs/superpowers/specs/2026-04-10-ts-protocol-decoder-design.md`

---

## File Structure

### New files (all under `server/ts-protocol/src/de/`)

| File | Responsibility |
|------|---------------|
| `mod.rs` | `decode()` entry point, `pub mod` declarations, macro definitions |
| `buf.rs` | `PacketBuf<'a>` cursor struct |
| `error.rs` | `DecodeError` enum, `DecodeResult<T>` type alias |
| `headers.rs` | `Pod` header structs + accessor impls for Ethernet, VLAN, SLL, SLL2, IPv4, IPv6, TCP |
| `l2.rs` | `decode_l2()`, `decode_ethernet()`, `strip_vlan()`, `decode_null()`, `detect_raw_ip()`, `decode_linux_sll()`, `decode_linux_sll2()` |
| `l3.rs` | `dispatch_l3()`, `decode_ipv4()`, `decode_ipv6()`, `L3Info` |
| `l4.rs` | `dispatch_l4()`, `decode_tcp()`, `L4Info` |

### Modified files

| File | Change |
|------|--------|
| `server/ts-protocol/Cargo.toml` | Add `bytemuck` dependency |
| `server/ts-protocol/src/lib.rs` | Add `pub mod de;` |
| `server/ts-protocol/src/net.rs` | Remove `parse_packet()`, `parse_ipv4_tcp()`, `parse_ipv6_tcp()`, `parse_tcp()`, `use ts_capture::RawPacket` |
| `server/ts-protocol/src/flow.rs` | Switch from `parse_packet(&raw)` to `de::decode(&raw.data, raw.link_type, raw.timestamp_us)` |
| `server/ts-capture/src/packet.rs` | Remove `ip_offset()`, `ethernet_ip_offset()`, constants, tests |
| `server/Cargo.toml` | Add `bytemuck` to workspace dependencies |

---

## Task 1: Add `bytemuck` dependency and scaffold `de` module

**Files:**
- Modify: `server/Cargo.toml`
- Modify: `server/ts-protocol/Cargo.toml`
- Create: `server/ts-protocol/src/de/mod.rs`
- Create: `server/ts-protocol/src/de/error.rs`
- Create: `server/ts-protocol/src/de/buf.rs`
- Create: `server/ts-protocol/src/de/headers.rs`
- Create: `server/ts-protocol/src/de/l2.rs`
- Create: `server/ts-protocol/src/de/l3.rs`
- Create: `server/ts-protocol/src/de/l4.rs`
- Modify: `server/ts-protocol/src/lib.rs`

- [ ] **Step 1: Add `bytemuck` to workspace dependencies**

In `server/Cargo.toml`, add to `[workspace.dependencies]`:

```toml
bytemuck = { version = "1", features = ["derive"] }
```

- [ ] **Step 2: Add `bytemuck` to ts-protocol dependencies**

In `server/ts-protocol/Cargo.toml`, add under `[dependencies]`:

```toml
bytemuck.workspace = true
```

- [ ] **Step 3: Create `error.rs`**

Create `server/ts-protocol/src/de/error.rs`:

```rust
/// Errors that can occur during packet decoding.
/// These are non-fatal — the caller simply skips the packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Not enough bytes for the expected header.
    Truncated,
    /// Protocol not handled (e.g., UDP, GRE).
    NotSupported,
    /// Link layer resolved to non-IP traffic (e.g., ARP).
    NotIp,
    /// Header field fails validation (e.g., IPv4 IHL < 5).
    InvalidHeader,
}

pub type DecodeResult<T> = Result<T, DecodeError>;
```

- [ ] **Step 4: Create `buf.rs`**

Create `server/ts-protocol/src/de/buf.rs`:

```rust
use bytemuck::Pod;
use std::mem::size_of;

/// A cursor over a raw packet byte slice for safe, zero-copy decoding.
pub struct PacketBuf<'a> {
    data: &'a [u8],
    offset: usize,
    /// Logical end (may be less than data.len() after set_len).
    len: usize,
}

impl<'a> PacketBuf<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            offset: 0,
            len: data.len(),
        }
    }

    /// Bytes remaining from current offset to logical end.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.len.saturating_sub(self.offset)
    }

    /// Current byte offset into the original data.
    #[inline]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Peek at the byte at the current offset without advancing.
    #[inline]
    pub fn peek(&self) -> Option<u8> {
        if self.offset < self.len {
            Some(self.data[self.offset])
        } else {
            None
        }
    }

    /// Zero-copy cast of bytes at the current offset to a Pod type.
    /// Returns `None` if there aren't enough bytes remaining.
    #[inline]
    pub fn get<H: Pod>(&self) -> Option<&'a H> {
        let size = size_of::<H>();
        if self.remaining() < size {
            return None;
        }
        let slice = &self.data[self.offset..self.offset + size];
        Some(bytemuck::from_bytes(slice))
    }

    /// Zero-copy cast + advance. Returns `None` if insufficient bytes.
    #[inline]
    pub fn consume<H: Pod>(&mut self) -> Option<&'a H> {
        let h = self.get::<H>()?;
        self.offset += size_of::<H>();
        Some(h)
    }

    /// Advance the offset by `n` bytes. Caller must ensure `n <= remaining()`.
    #[inline]
    pub fn advance(&mut self, n: usize) {
        self.offset += n;
    }

    /// Return the remaining bytes as a slice.
    #[inline]
    pub fn remaining_slice(&self) -> &'a [u8] {
        if self.offset >= self.len {
            &[]
        } else {
            &self.data[self.offset..self.len]
        }
    }

    /// Truncate the logical length. Used for IPv4 total_length to strip padding.
    /// `new_len` is an absolute position, not relative to offset.
    #[inline]
    pub fn set_len(&mut self, new_len: usize) {
        if new_len < self.len {
            self.len = new_len;
        }
    }
}
```

- [ ] **Step 5: Create `headers.rs`**

Create `server/ts-protocol/src/de/headers.rs`:

```rust
use std::net::{Ipv4Addr, Ipv6Addr};

use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// Link-layer constants
// ---------------------------------------------------------------------------

pub const LINKTYPE_NULL: u32 = 0;
pub const LINKTYPE_ETHERNET: u32 = 1;
pub const LINKTYPE_RAW: u32 = 101;
pub const LINKTYPE_LINUX_SLL: u32 = 113;
pub const LINKTYPE_LINUX_SLL2: u32 = 276;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_IPV6: u16 = 0x86DD;
pub const ETHERTYPE_VLAN: u16 = 0x8100;
pub const ETHERTYPE_QINQ: u16 = 0x88A8;

pub const IP_PROTO_TCP: u8 = 6;

// BSD loopback AF values (host byte order — little-endian on x86/ARM).
pub const AF_INET: u32 = 2;
pub const AF_INET6_BSD: u32 = 30;   // macOS / FreeBSD
pub const AF_INET6_LINUX: u32 = 10; // Linux

// ---------------------------------------------------------------------------
// L2 Headers
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct EthernetHeader {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub ether_type: [u8; 2],
}

impl EthernetHeader {
    #[inline]
    pub fn ether_type(&self) -> u16 {
        u16::from_be_bytes(self.ether_type)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct VlanHeader {
    pub tci: [u8; 2],
    pub ether_type: [u8; 2],
}

impl VlanHeader {
    #[inline]
    pub fn ether_type(&self) -> u16 {
        u16::from_be_bytes(self.ether_type)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LinuxSllHeader {
    pub packet_type: [u8; 2],
    pub arphrd_type: [u8; 2],
    pub addr_len: [u8; 2],
    pub addr: [u8; 8],
    pub protocol: [u8; 2],
}

impl LinuxSllHeader {
    #[inline]
    pub fn protocol(&self) -> u16 {
        u16::from_be_bytes(self.protocol)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LinuxSll2Header {
    pub protocol: [u8; 2],
    pub _reserved: [u8; 2],
    pub iface_index: [u8; 4],
    pub arphrd_type: [u8; 2],
    pub packet_type: u8,
    pub addr_len: u8,
    pub addr: [u8; 8],
}

impl LinuxSll2Header {
    #[inline]
    pub fn protocol(&self) -> u16 {
        u16::from_be_bytes(self.protocol)
    }
}

/// 4-byte BSD loopback header (LINKTYPE_NULL). Contains AF family in host byte order.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct NullHeader {
    pub af_family: [u8; 4],
}

impl NullHeader {
    #[inline]
    pub fn af_family(&self) -> u32 {
        // BSD null/loopback uses host byte order (little-endian on common platforms).
        u32::from_ne_bytes(self.af_family)
    }
}

// ---------------------------------------------------------------------------
// L3 Headers
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Ipv4Header {
    pub ver_ihl: u8,
    pub tos: u8,
    pub total_length: [u8; 2],
    pub identification: [u8; 2],
    pub flags_frag: [u8; 2],
    pub ttl: u8,
    pub protocol: u8,
    pub checksum: [u8; 2],
    pub src: [u8; 4],
    pub dst: [u8; 4],
}

impl Ipv4Header {
    /// Internet Header Length in bytes (IHL field * 4).
    #[inline]
    pub fn ihl(&self) -> usize {
        ((self.ver_ihl & 0x0F) as usize) * 4
    }

    #[inline]
    pub fn total_length(&self) -> u16 {
        u16::from_be_bytes(self.total_length)
    }

    #[inline]
    pub fn protocol(&self) -> u8 {
        self.protocol
    }

    #[inline]
    pub fn src_ip(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.src)
    }

    #[inline]
    pub fn dst_ip(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.dst)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Ipv6Header {
    pub ver_tc_fl: [u8; 4],
    pub payload_length: [u8; 2],
    pub next_header: u8,
    pub hop_limit: u8,
    pub src: [u8; 16],
    pub dst: [u8; 16],
}

impl Ipv6Header {
    #[inline]
    pub fn payload_length(&self) -> u16 {
        u16::from_be_bytes(self.payload_length)
    }

    #[inline]
    pub fn next_header(&self) -> u8 {
        self.next_header
    }

    #[inline]
    pub fn src_ip(&self) -> Ipv6Addr {
        Ipv6Addr::from(self.src)
    }

    #[inline]
    pub fn dst_ip(&self) -> Ipv6Addr {
        Ipv6Addr::from(self.dst)
    }
}

// ---------------------------------------------------------------------------
// L4 Headers
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct TcpHeader {
    pub src_port: [u8; 2],
    pub dst_port: [u8; 2],
    pub seq: [u8; 4],
    pub ack: [u8; 4],
    pub data_offset_flags: [u8; 2],
    pub window: [u8; 2],
    pub checksum: [u8; 2],
    pub urgent: [u8; 2],
}

impl TcpHeader {
    #[inline]
    pub fn src_port(&self) -> u16 {
        u16::from_be_bytes(self.src_port)
    }

    #[inline]
    pub fn dst_port(&self) -> u16 {
        u16::from_be_bytes(self.dst_port)
    }

    #[inline]
    pub fn seq(&self) -> u32 {
        u32::from_be_bytes(self.seq)
    }

    #[inline]
    pub fn ack(&self) -> u32 {
        u32::from_be_bytes(self.ack)
    }

    /// Data offset in bytes (data_offset field * 4).
    #[inline]
    pub fn data_offset(&self) -> usize {
        ((self.data_offset_flags[0] >> 4) as usize) * 4
    }

    #[inline]
    pub fn flags(&self) -> u8 {
        self.data_offset_flags[1]
    }
}
```

- [ ] **Step 6: Create stub `l2.rs`, `l3.rs`, `l4.rs`**

Create `server/ts-protocol/src/de/l2.rs`:

```rust
use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;

/// Decode the link layer and return the EtherType for L3 dispatch.
pub fn decode_l2(_buf: &mut PacketBuf, _link_type: u32) -> DecodeResult<u16> {
    Err(DecodeError::NotSupported) // placeholder
}
```

Create `server/ts-protocol/src/de/l3.rs`:

```rust
use std::net::IpAddr;

use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;

pub struct L3Info {
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub protocol: u8,
}

/// Dispatch by EtherType, decode IP header.
pub fn dispatch_l3(_buf: &mut PacketBuf, _ether_type: u16) -> DecodeResult<L3Info> {
    Err(DecodeError::NotSupported) // placeholder
}
```

Create `server/ts-protocol/src/de/l4.rs`:

```rust
use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;

pub struct L4Info {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
}

/// Dispatch by IP protocol number, decode transport header.
pub fn dispatch_l4(_buf: &mut PacketBuf, _protocol: u8) -> DecodeResult<L4Info> {
    Err(DecodeError::NotSupported) // placeholder
}
```

- [ ] **Step 7: Create `mod.rs` with macros and `decode()` stub**

Create `server/ts-protocol/src/de/mod.rs`:

```rust
pub mod buf;
pub mod error;
pub mod headers;
pub mod l2;
pub mod l3;
pub mod l4;

use bytes::Bytes;

use crate::net::{Direction, FlowKey, ParsedPacket};
use l2::decode_l2;
use l3::dispatch_l3;
use l4::dispatch_l4;

use self::buf::PacketBuf;

/// Consume a `Pod` header from `buf`, or return `DecodeError::Truncated`.
macro_rules! try_consume {
    ($buf:expr, $H:ty) => {
        $buf.consume::<$H>()
            .ok_or(crate::de::error::DecodeError::Truncated)?
    };
}

/// Skip `$n` bytes from `buf`, or return `DecodeError::Truncated`.
macro_rules! try_skip {
    ($buf:expr, $n:expr) => {
        if $buf.remaining() >= $n {
            $buf.advance($n);
        } else {
            return Err(crate::de::error::DecodeError::Truncated);
        }
    };
}

pub(crate) use try_consume;
pub(crate) use try_skip;

/// Decode raw packet bytes into a `ParsedPacket`.
/// Returns `None` for non-TCP, unsupported, or malformed packets.
pub fn decode(data: &[u8], link_type: u32, timestamp_us: i64) -> Option<ParsedPacket> {
    let mut buf = PacketBuf::new(data);

    let ether_type = decode_l2(&mut buf, link_type).ok()?;
    let l3 = dispatch_l3(&mut buf, ether_type).ok()?;
    let l4 = dispatch_l4(&mut buf, l3.protocol).ok()?;

    let payload = Bytes::copy_from_slice(buf.remaining_slice());
    let flow_key = FlowKey::new(l3.src_ip, l4.src_port, l3.dst_ip, l4.dst_port);
    let direction = if (l3.src_ip, l4.src_port) <= (l3.dst_ip, l4.dst_port) {
        Direction::AtoB
    } else {
        Direction::BtoA
    };

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

- [ ] **Step 8: Register the module in `lib.rs`**

In `server/ts-protocol/src/lib.rs`, add `pub mod de;` after the existing module declarations:

```rust
pub mod de;
pub mod flow;
pub mod http;
pub mod model;
pub mod net;
pub mod pipeline;
pub mod tcp;
```

- [ ] **Step 9: Verify it compiles**

Run: `cargo check --manifest-path server/Cargo.toml -p ts-protocol`

Expected: Compiles with warnings about unused imports/variables in stubs.

- [ ] **Step 10: Commit**

```bash
git add server/Cargo.toml server/ts-protocol/Cargo.toml server/ts-protocol/src/lib.rs server/ts-protocol/src/de/
git commit -m "$(cat <<'EOF'
refactor(ts-protocol): scaffold de/ decoder submodule

Add bytemuck dependency, PacketBuf cursor, DecodeError, Pod header
structs (Ethernet, VLAN, SLL, SLL2, IPv4, IPv6, TCP), and stub
decoder functions. decode() entry point wired but dispatchers return
NotSupported until implemented.
EOF
)"
```

---

## Task 2: Implement and test `PacketBuf`

**Files:**
- Modify: `server/ts-protocol/src/de/buf.rs`

- [ ] **Step 1: Write `PacketBuf` tests**

Add to the bottom of `server/ts-protocol/src/de/buf.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_buf_has_correct_initial_state() {
        let data = [0u8; 40];
        let buf = PacketBuf::new(&data);
        assert_eq!(buf.remaining(), 40);
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn peek_returns_first_byte_without_advancing() {
        let data = [0xAB, 0xCD];
        let buf = PacketBuf::new(&data);
        assert_eq!(buf.peek(), Some(0xAB));
        assert_eq!(buf.offset(), 0);
    }

    #[test]
    fn peek_empty_returns_none() {
        let data = [];
        let buf = PacketBuf::new(&data);
        assert_eq!(buf.peek(), None);
    }

    #[test]
    fn consume_advances_offset() {
        #[repr(C)]
        #[derive(Clone, Copy, Pod, Zeroable)]
        struct TwoBytes {
            a: u8,
            b: u8,
        }

        let data = [0x11, 0x22, 0x33];
        let mut buf = PacketBuf::new(&data);
        let h = buf.consume::<TwoBytes>().unwrap();
        assert_eq!(h.a, 0x11);
        assert_eq!(h.b, 0x22);
        assert_eq!(buf.offset(), 2);
        assert_eq!(buf.remaining(), 1);
    }

    #[test]
    fn consume_insufficient_bytes_returns_none() {
        #[repr(C)]
        #[derive(Clone, Copy, Pod, Zeroable)]
        struct FourBytes {
            val: [u8; 4],
        }

        let data = [0x11, 0x22];
        let mut buf = PacketBuf::new(&data);
        assert!(buf.consume::<FourBytes>().is_none());
        assert_eq!(buf.offset(), 0); // offset unchanged
    }

    #[test]
    fn advance_and_remaining_slice() {
        let data = [0x01, 0x02, 0x03, 0x04];
        let mut buf = PacketBuf::new(&data);
        buf.advance(2);
        assert_eq!(buf.remaining_slice(), &[0x03, 0x04]);
    }

    #[test]
    fn set_len_truncates() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05];
        let mut buf = PacketBuf::new(&data);
        buf.advance(1);
        buf.set_len(3); // logical end at byte 3
        assert_eq!(buf.remaining(), 2);
        assert_eq!(buf.remaining_slice(), &[0x02, 0x03]);
    }

    #[test]
    fn set_len_cannot_grow() {
        let data = [0x01, 0x02];
        let mut buf = PacketBuf::new(&data);
        buf.set_len(100);
        assert_eq!(buf.remaining(), 2); // unchanged
    }

    #[test]
    fn remaining_slice_empty_when_exhausted() {
        let data = [0x01, 0x02];
        let mut buf = PacketBuf::new(&data);
        buf.advance(2);
        assert_eq!(buf.remaining_slice(), &[]);
        assert_eq!(buf.remaining(), 0);
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-protocol de::buf`

Expected: All 8 tests PASS (PacketBuf is already implemented in Task 1).

- [ ] **Step 3: Commit**

```bash
git add server/ts-protocol/src/de/buf.rs
git commit -m "$(cat <<'EOF'
test(ts-protocol): add PacketBuf unit tests

Cover new/peek/consume/advance/remaining_slice/set_len, including
edge cases for empty buffers and insufficient bytes.
EOF
)"
```

---

## Task 3: Implement and test L2 decoders

**Files:**
- Modify: `server/ts-protocol/src/de/l2.rs`

- [ ] **Step 1: Write L2 tests**

Replace `server/ts-protocol/src/de/l2.rs` with:

```rust
use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;
use super::{try_consume, try_skip};

/// Decode the link layer and return the EtherType for L3 dispatch.
pub fn decode_l2(buf: &mut PacketBuf, link_type: u32) -> DecodeResult<u16> {
    match link_type {
        LINKTYPE_ETHERNET => decode_ethernet(buf),
        LINKTYPE_RAW => detect_raw_ip(buf),
        LINKTYPE_NULL => decode_null(buf),
        LINKTYPE_LINUX_SLL => decode_linux_sll(buf),
        LINKTYPE_LINUX_SLL2 => decode_linux_sll2(buf),
        _ => Err(DecodeError::NotSupported),
    }
}

fn decode_ethernet(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let eth = try_consume!(buf, EthernetHeader);
    let ether_type = eth.ether_type();
    strip_vlan(buf, ether_type)
}

fn strip_vlan(buf: &mut PacketBuf, ether_type: u16) -> DecodeResult<u16> {
    match ether_type {
        ETHERTYPE_VLAN | ETHERTYPE_QINQ => {
            let vlan = try_consume!(buf, VlanHeader);
            let inner = vlan.ether_type();
            strip_vlan(buf, inner)
        }
        other => Ok(other),
    }
}

fn decode_null(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let hdr = try_consume!(buf, NullHeader);
    match hdr.af_family() {
        AF_INET => Ok(ETHERTYPE_IPV4),
        AF_INET6_BSD | AF_INET6_LINUX => Ok(ETHERTYPE_IPV6),
        _ => Err(DecodeError::NotIp),
    }
}

fn detect_raw_ip(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let first = buf.peek().ok_or(DecodeError::Truncated)?;
    match first >> 4 {
        4 => Ok(ETHERTYPE_IPV4),
        6 => Ok(ETHERTYPE_IPV6),
        _ => Err(DecodeError::NotIp),
    }
}

fn decode_linux_sll(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let hdr = try_consume!(buf, LinuxSllHeader);
    let proto = hdr.protocol();
    if proto == ETHERTYPE_IPV4 || proto == ETHERTYPE_IPV6 {
        Ok(proto)
    } else {
        Err(DecodeError::NotIp)
    }
}

fn decode_linux_sll2(buf: &mut PacketBuf) -> DecodeResult<u16> {
    let hdr = try_consume!(buf, LinuxSll2Header);
    let proto = hdr.protocol();
    if proto == ETHERTYPE_IPV4 || proto == ETHERTYPE_IPV6 {
        Ok(proto)
    } else {
        Err(DecodeError::NotIp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Ethernet ---

    #[test]
    fn ethernet_ipv4() {
        let mut data = vec![0u8; 12]; // dst + src MAC
        data.extend_from_slice(&[0x08, 0x00]); // IPv4
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 14);
    }

    #[test]
    fn ethernet_ipv6() {
        let mut data = vec![0u8; 12];
        data.extend_from_slice(&[0x86, 0xDD]);
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn ethernet_vlan_ipv4() {
        let mut data = vec![0u8; 12];
        data.extend_from_slice(&[0x81, 0x00]); // VLAN
        data.extend_from_slice(&[0x00, 0x01]); // TCI
        data.extend_from_slice(&[0x08, 0x00]); // inner IPv4
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 18); // 14 + 4
    }

    #[test]
    fn ethernet_qinq_ipv4() {
        let mut data = vec![0u8; 12];
        data.extend_from_slice(&[0x88, 0xA8]); // QinQ
        data.extend_from_slice(&[0x00, 0x01]); // outer TCI
        data.extend_from_slice(&[0x81, 0x00]); // inner VLAN
        data.extend_from_slice(&[0x00, 0x02]); // inner TCI
        data.extend_from_slice(&[0x08, 0x00]); // IPv4
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 22); // 14 + 4 + 4
    }

    #[test]
    fn ethernet_arp_returns_not_ip() {
        let mut data = vec![0u8; 12];
        data.extend_from_slice(&[0x08, 0x06]); // ARP
        let mut buf = PacketBuf::new(&data);
        // ARP is not an error — it's just non-IP. The caller gets Ok(0x0806)
        // and dispatch_l3 will return NotIp.
        let result = decode_l2(&mut buf, LINKTYPE_ETHERNET);
        assert_eq!(result, Ok(0x0806));
    }

    #[test]
    fn ethernet_truncated() {
        let data = [0u8; 10]; // less than 14-byte Ethernet header
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Err(DecodeError::Truncated));
    }

    #[test]
    fn vlan_truncated() {
        let mut data = vec![0u8; 12];
        data.extend_from_slice(&[0x81, 0x00]); // VLAN
        data.push(0x00); // only 1 byte of VLAN header (need 4)
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Err(DecodeError::Truncated));
    }

    // --- Raw IP ---

    #[test]
    fn raw_ip_v4() {
        let data = [0x45]; // version nibble = 4
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_RAW), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 0); // peek doesn't advance
    }

    #[test]
    fn raw_ip_v6() {
        let data = [0x60]; // version nibble = 6
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_RAW), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn raw_ip_empty() {
        let data = [];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_RAW), Err(DecodeError::Truncated));
    }

    // --- BSD Loopback (NULL) ---

    #[test]
    fn null_ipv4() {
        let data = AF_INET.to_ne_bytes();
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_NULL), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 4);
    }

    #[test]
    fn null_ipv6_bsd() {
        let data = AF_INET6_BSD.to_ne_bytes();
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_NULL), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn null_non_ip() {
        let data = 999u32.to_ne_bytes();
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_NULL), Err(DecodeError::NotIp));
    }

    // --- Linux SLL ---

    #[test]
    fn sll_ipv4() {
        let mut data = [0u8; 16];
        data[14] = 0x08; // protocol high byte
        data[15] = 0x00; // protocol low byte = IPv4
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_LINUX_SLL), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 16);
    }

    #[test]
    fn sll_non_ip() {
        let mut data = [0u8; 16];
        data[14] = 0x08;
        data[15] = 0x06; // ARP
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_LINUX_SLL), Err(DecodeError::NotIp));
    }

    #[test]
    fn sll_truncated() {
        let data = [0u8; 10];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_LINUX_SLL), Err(DecodeError::Truncated));
    }

    // --- Linux SLL2 ---

    #[test]
    fn sll2_ipv4() {
        let mut data = [0u8; 20];
        data[0] = 0x08;
        data[1] = 0x00; // IPv4
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_LINUX_SLL2), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 20);
    }

    #[test]
    fn sll2_non_ip() {
        let mut data = [0u8; 20];
        data[0] = 0x08;
        data[1] = 0x06; // ARP
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_LINUX_SLL2), Err(DecodeError::NotIp));
    }

    // --- Unsupported link type ---

    #[test]
    fn unsupported_link_type() {
        let data = [0u8; 20];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, 999), Err(DecodeError::NotSupported));
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-protocol de::l2`

Expected: All 16 L2 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add server/ts-protocol/src/de/l2.rs
git commit -m "$(cat <<'EOF'
feat(ts-protocol): implement L2 decoders

Ethernet, VLAN/QinQ stripping, Raw IP detection, BSD loopback (NULL),
Linux cooked capture SLL/SLL2. All dispatched via decode_l2().
EOF
)"
```

---

## Task 4: Implement and test L3 decoders

**Files:**
- Modify: `server/ts-protocol/src/de/l3.rs`

- [ ] **Step 1: Implement L3 decoders with tests**

Replace `server/ts-protocol/src/de/l3.rs` with:

```rust
use std::net::IpAddr;

use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;
use super::{try_consume, try_skip};

/// Information extracted from the network layer.
pub struct L3Info {
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub protocol: u8,
}

/// Dispatch by EtherType, decode IP header.
pub fn dispatch_l3(buf: &mut PacketBuf, ether_type: u16) -> DecodeResult<L3Info> {
    match ether_type {
        ETHERTYPE_IPV4 => decode_ipv4(buf),
        ETHERTYPE_IPV6 => decode_ipv6(buf),
        _ => Err(DecodeError::NotIp),
    }
}

fn decode_ipv4(buf: &mut PacketBuf) -> DecodeResult<L3Info> {
    let ip = try_consume!(buf, Ipv4Header);
    let ihl = ip.ihl();

    // Minimum IPv4 header is 20 bytes. The Ipv4Header struct is already 20 bytes,
    // so ihl < 20 means the field is invalid.
    if ihl < 20 {
        return Err(DecodeError::InvalidHeader);
    }

    // Skip IP options (bytes beyond the fixed 20-byte header).
    let options_len = ihl - 20;
    if options_len > 0 {
        try_skip!(buf, options_len);
    }

    // Truncate buffer to IP total_length to strip any link-layer padding.
    // ip_start is where the IPv4 header began (before we consumed it).
    let ip_start = buf.offset() - ihl;
    let total_len = ip.total_length() as usize;
    if total_len >= ihl {
        buf.set_len(ip_start + total_len);
    }

    Ok(L3Info {
        src_ip: IpAddr::V4(ip.src_ip()),
        dst_ip: IpAddr::V4(ip.dst_ip()),
        protocol: ip.protocol(),
    })
}

fn decode_ipv6(buf: &mut PacketBuf) -> DecodeResult<L3Info> {
    let ip = try_consume!(buf, Ipv6Header);

    // Truncate buffer to IPv6 payload_length.
    let ip_start = buf.offset() - 40; // Ipv6Header is always 40 bytes
    let total_len = 40 + ip.payload_length() as usize;
    buf.set_len(ip_start + total_len);

    Ok(L3Info {
        src_ip: IpAddr::V6(ip.src_ip()),
        dst_ip: IpAddr::V6(ip.dst_ip()),
        protocol: ip.next_header(),
    })
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    /// Build a minimal IPv4 header (20 bytes, no options).
    fn make_ipv4(protocol: u8, src: [u8; 4], dst: [u8; 4], total_length: u16) -> Vec<u8> {
        let mut h = vec![0u8; 20];
        h[0] = 0x45; // version=4, ihl=5 (20 bytes)
        let tl = total_length.to_be_bytes();
        h[2] = tl[0];
        h[3] = tl[1];
        h[9] = protocol;
        h[12..16].copy_from_slice(&src);
        h[16..20].copy_from_slice(&dst);
        h
    }

    /// Build a minimal IPv6 header (40 bytes).
    fn make_ipv6(next_header: u8, src: [u8; 16], dst: [u8; 16], payload_length: u16) -> Vec<u8> {
        let mut h = vec![0u8; 40];
        h[0] = 0x60; // version=6
        let pl = payload_length.to_be_bytes();
        h[4] = pl[0];
        h[5] = pl[1];
        h[6] = next_header;
        h[8..24].copy_from_slice(&src);
        h[24..40].copy_from_slice(&dst);
        h
    }

    #[test]
    fn ipv4_standard() {
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 40);
        data.extend_from_slice(&[0u8; 20]); // TCP payload space
        let mut buf = PacketBuf::new(&data);
        let l3 = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap();
        assert_eq!(l3.src_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(l3.dst_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(l3.protocol, 6);
        assert_eq!(buf.offset(), 20);
    }

    #[test]
    fn ipv4_with_options() {
        // IHL = 6 (24 bytes), 4 bytes of options
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 44);
        data[0] = 0x46; // ihl = 6
        data.splice(20..20, [0u8; 4]); // insert 4 option bytes
        data.extend_from_slice(&[0u8; 20]); // TCP space
        let mut buf = PacketBuf::new(&data);
        let l3 = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap();
        assert_eq!(l3.protocol, 6);
        assert_eq!(buf.offset(), 24); // 20 header + 4 options
    }

    #[test]
    fn ipv4_invalid_ihl() {
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 20);
        data[0] = 0x43; // ihl = 3 (12 bytes < 20 minimum)
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l3(&mut buf, ETHERTYPE_IPV4), Err(DecodeError::InvalidHeader));
    }

    #[test]
    fn ipv4_total_length_truncates_padding() {
        // 20-byte IPv4 header, total_length = 30, but captured 40 bytes
        let mut data = make_ipv4(6, [10, 0, 0, 1], [10, 0, 0, 2], 30);
        data.extend_from_slice(&[0xAA; 20]); // extra data (10 real + 10 padding)
        let mut buf = PacketBuf::new(&data);
        let _ = dispatch_l3(&mut buf, ETHERTYPE_IPV4).unwrap();
        assert_eq!(buf.remaining(), 10); // 30 - 20 = 10 bytes of payload
    }

    #[test]
    fn ipv4_truncated() {
        let data = [0x45, 0x00]; // only 2 bytes, need 20
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l3(&mut buf, ETHERTYPE_IPV4), Err(DecodeError::Truncated));
    }

    #[test]
    fn ipv6_standard() {
        let src = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let dst = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let mut data = make_ipv6(6, src, dst, 20);
        data.extend_from_slice(&[0u8; 20]); // TCP space
        let mut buf = PacketBuf::new(&data);
        let l3 = dispatch_l3(&mut buf, ETHERTYPE_IPV6).unwrap();
        assert_eq!(l3.src_ip, IpAddr::V6(Ipv6Addr::from(src)));
        assert_eq!(l3.dst_ip, IpAddr::V6(Ipv6Addr::from(dst)));
        assert_eq!(l3.protocol, 6);
        assert_eq!(buf.offset(), 40);
    }

    #[test]
    fn ipv6_truncated() {
        let data = [0x60; 20]; // only 20 bytes, need 40
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l3(&mut buf, ETHERTYPE_IPV6), Err(DecodeError::Truncated));
    }

    #[test]
    fn non_ip_ether_type() {
        let data = [0u8; 40];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l3(&mut buf, 0x0806), Err(DecodeError::NotIp)); // ARP
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-protocol de::l3`

Expected: All 8 L3 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add server/ts-protocol/src/de/l3.rs
git commit -m "$(cat <<'EOF'
feat(ts-protocol): implement L3 decoders

IPv4 with IHL validation, options skip, total_length truncation.
IPv6 with payload_length truncation. dispatch_l3() routes by EtherType.
EOF
)"
```

---

## Task 5: Implement and test L4 decoder (TCP)

**Files:**
- Modify: `server/ts-protocol/src/de/l4.rs`

- [ ] **Step 1: Implement L4 decoder with tests**

Replace `server/ts-protocol/src/de/l4.rs` with:

```rust
use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;
use super::{try_consume, try_skip};

/// Information extracted from the transport layer.
pub struct L4Info {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
}

/// Dispatch by IP protocol number.
pub fn dispatch_l4(buf: &mut PacketBuf, protocol: u8) -> DecodeResult<L4Info> {
    match protocol {
        IP_PROTO_TCP => decode_tcp(buf),
        _ => Err(DecodeError::NotSupported),
    }
}

fn decode_tcp(buf: &mut PacketBuf) -> DecodeResult<L4Info> {
    let tcp = try_consume!(buf, TcpHeader);
    let data_offset = tcp.data_offset();

    // Minimum TCP header is 20 bytes.
    if data_offset < 20 {
        return Err(DecodeError::InvalidHeader);
    }

    // Skip TCP options (bytes beyond the fixed 20-byte header).
    let options_len = data_offset - 20;
    if options_len > 0 {
        try_skip!(buf, options_len);
    }

    Ok(L4Info {
        src_port: tcp.src_port(),
        dst_port: tcp.dst_port(),
        seq: tcp.seq(),
        ack: tcp.ack(),
        flags: tcp.flags(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal TCP header (20 bytes, no options).
    fn make_tcp(src_port: u16, dst_port: u16, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
        let mut h = vec![0u8; 20];
        let sp = src_port.to_be_bytes();
        h[0] = sp[0];
        h[1] = sp[1];
        let dp = dst_port.to_be_bytes();
        h[2] = dp[0];
        h[3] = dp[1];
        let s = seq.to_be_bytes();
        h[4..8].copy_from_slice(&s);
        let a = ack.to_be_bytes();
        h[8..12].copy_from_slice(&a);
        h[12] = 0x50; // data_offset = 5 (20 bytes)
        h[13] = flags;
        h
    }

    #[test]
    fn tcp_standard() {
        let mut data = make_tcp(12345, 80, 100, 200, 0x12); // SYN+ACK
        data.extend_from_slice(b"payload");
        let mut buf = PacketBuf::new(&data);
        let l4 = dispatch_l4(&mut buf, IP_PROTO_TCP).unwrap();
        assert_eq!(l4.src_port, 12345);
        assert_eq!(l4.dst_port, 80);
        assert_eq!(l4.seq, 100);
        assert_eq!(l4.ack, 200);
        assert_eq!(l4.flags, 0x12);
        assert_eq!(buf.remaining_slice(), b"payload");
    }

    #[test]
    fn tcp_with_options() {
        let mut data = make_tcp(1000, 443, 0, 0, 0x02);
        data[12] = 0x80; // data_offset = 8 (32 bytes)
        data.extend_from_slice(&[0u8; 12]); // 12 bytes of options
        data.extend_from_slice(b"data");
        let mut buf = PacketBuf::new(&data);
        let l4 = dispatch_l4(&mut buf, IP_PROTO_TCP).unwrap();
        assert_eq!(l4.src_port, 1000);
        assert_eq!(buf.remaining_slice(), b"data");
        assert_eq!(buf.offset(), 32); // 20 header + 12 options
    }

    #[test]
    fn tcp_invalid_data_offset() {
        let mut data = make_tcp(1000, 80, 0, 0, 0x02);
        data[12] = 0x30; // data_offset = 3 (12 bytes < 20 minimum)
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l4(&mut buf, IP_PROTO_TCP), Err(DecodeError::InvalidHeader));
    }

    #[test]
    fn tcp_truncated() {
        let data = [0u8; 10]; // need 20
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l4(&mut buf, IP_PROTO_TCP), Err(DecodeError::Truncated));
    }

    #[test]
    fn tcp_options_truncated() {
        let mut data = make_tcp(1000, 80, 0, 0, 0x02);
        data[12] = 0x80; // data_offset = 8 (32 bytes), but no option bytes follow
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l4(&mut buf, IP_PROTO_TCP), Err(DecodeError::Truncated));
    }

    #[test]
    fn udp_not_supported() {
        let data = [0u8; 20];
        let mut buf = PacketBuf::new(&data);
        assert_eq!(dispatch_l4(&mut buf, 17), Err(DecodeError::NotSupported)); // UDP
    }

    #[test]
    fn tcp_no_payload() {
        let data = make_tcp(80, 443, 500, 600, 0x10); // ACK only, no payload
        let mut buf = PacketBuf::new(&data);
        let l4 = dispatch_l4(&mut buf, IP_PROTO_TCP).unwrap();
        assert_eq!(l4.flags, 0x10);
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.remaining_slice(), &[]);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-protocol de::l4`

Expected: All 7 L4 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add server/ts-protocol/src/de/l4.rs
git commit -m "$(cat <<'EOF'
feat(ts-protocol): implement L4 TCP decoder

TCP header parsing with data_offset validation, options skip.
dispatch_l4() routes by IP protocol number.
EOF
)"
```

---

## Task 6: Implement and test full `decode()` integration

**Files:**
- Modify: `server/ts-protocol/src/de/mod.rs`

- [ ] **Step 1: Add integration tests to `mod.rs`**

Add at the bottom of `server/ts-protocol/src/de/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::net::Direction;
    use headers::*;

    /// Helpers to build packet byte arrays layer by layer.

    fn ethernet_hdr(ether_type: u16) -> Vec<u8> {
        let mut h = vec![0u8; 12]; // dst + src MAC
        h.extend_from_slice(&ether_type.to_be_bytes());
        h
    }

    fn vlan_tag(ether_type: u16) -> Vec<u8> {
        let mut h = vec![0x00, 0x01]; // TCI
        h.extend_from_slice(&ether_type.to_be_bytes());
        h
    }

    fn ipv4_hdr(protocol: u8, src: [u8; 4], dst: [u8; 4], payload_len: usize) -> Vec<u8> {
        let total_length = (20 + payload_len) as u16;
        let mut h = vec![0u8; 20];
        h[0] = 0x45;
        let tl = total_length.to_be_bytes();
        h[2] = tl[0];
        h[3] = tl[1];
        h[9] = protocol;
        h[12..16].copy_from_slice(&src);
        h[16..20].copy_from_slice(&dst);
        h
    }

    fn ipv6_hdr(next_header: u8, src: [u8; 16], dst: [u8; 16], payload_len: u16) -> Vec<u8> {
        let mut h = vec![0u8; 40];
        h[0] = 0x60;
        let pl = payload_len.to_be_bytes();
        h[4] = pl[0];
        h[5] = pl[1];
        h[6] = next_header;
        h[8..24].copy_from_slice(&src);
        h[24..40].copy_from_slice(&dst);
        h
    }

    fn tcp_hdr(src_port: u16, dst_port: u16, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
        let mut h = vec![0u8; 20];
        let sp = src_port.to_be_bytes();
        h[0] = sp[0];
        h[1] = sp[1];
        let dp = dst_port.to_be_bytes();
        h[2] = dp[0];
        h[3] = dp[1];
        let s = seq.to_be_bytes();
        h[4..8].copy_from_slice(&s);
        let a = ack.to_be_bytes();
        h[8..12].copy_from_slice(&a);
        h[12] = 0x50; // data_offset = 5
        h[13] = flags;
        h
    }

    #[test]
    fn decode_eth_ipv4_tcp() {
        let tcp_payload = b"GET / HTTP/1.1\r\n";
        let tcp = tcp_hdr(12345, 80, 1000, 0, 0x18); // PSH+ACK
        let ip = ipv4_hdr(6, [10, 0, 0, 1], [10, 0, 0, 2], tcp.len() + tcp_payload.len());
        let eth = ethernet_hdr(ETHERTYPE_IPV4);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);
        pkt.extend_from_slice(tcp_payload);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 1234567890).unwrap();
        assert_eq!(result.src_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(result.dst_ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(result.src_port, 12345);
        assert_eq!(result.dst_port, 80);
        assert_eq!(result.tcp_seq, 1000);
        assert_eq!(result.tcp_flags, 0x18);
        assert_eq!(result.payload, &tcp_payload[..]);
        assert_eq!(result.timestamp_us, 1234567890);
        assert_eq!(result.direction, Direction::AtoB);
    }

    #[test]
    fn decode_eth_ipv6_tcp() {
        let tcp_payload = b"hello";
        let tcp = tcp_hdr(443, 50000, 0, 0, 0x02); // SYN
        let src = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let dst = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let ip = ipv6_hdr(6, src, dst, (tcp.len() + tcp_payload.len()) as u16);
        let eth = ethernet_hdr(ETHERTYPE_IPV6);

        let mut pkt = eth;
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);
        pkt.extend_from_slice(tcp_payload);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 99).unwrap();
        assert_eq!(result.src_ip, IpAddr::V6(Ipv6Addr::from(src)));
        assert_eq!(result.dst_ip, IpAddr::V6(Ipv6Addr::from(dst)));
        assert_eq!(result.src_port, 443);
        assert_eq!(result.dst_port, 50000);
        assert_eq!(result.payload, &tcp_payload[..]);
    }

    #[test]
    fn decode_vlan_ipv4_tcp() {
        let tcp = tcp_hdr(80, 443, 0, 0, 0x10);
        let ip = ipv4_hdr(6, [192, 168, 1, 1], [192, 168, 1, 2], tcp.len());
        let mut pkt = ethernet_hdr(ETHERTYPE_VLAN);
        pkt.extend_from_slice(&vlan_tag(ETHERTYPE_IPV4));
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 0).unwrap();
        assert_eq!(result.src_ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(result.src_port, 80);
    }

    #[test]
    fn decode_raw_ipv4_tcp() {
        let tcp = tcp_hdr(8080, 80, 0, 0, 0x02);
        let ip = ipv4_hdr(6, [1, 2, 3, 4], [5, 6, 7, 8], tcp.len());
        let mut pkt = ip;
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_RAW, 0).unwrap();
        assert_eq!(result.src_ip, IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
        assert_eq!(result.src_port, 8080);
    }

    #[test]
    fn decode_sll_ipv4_tcp() {
        let tcp = tcp_hdr(9090, 80, 0, 0, 0x02);
        let ip = ipv4_hdr(6, [10, 0, 0, 1], [10, 0, 0, 2], tcp.len());
        let mut pkt = vec![0u8; 14]; // SLL header first 14 bytes
        pkt.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes()); // protocol at 14-15
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_LINUX_SLL, 0).unwrap();
        assert_eq!(result.src_port, 9090);
    }

    #[test]
    fn decode_direction_btoa() {
        // src > dst, so direction should be BtoA
        let tcp = tcp_hdr(80, 443, 0, 0, 0x10);
        let ip = ipv4_hdr(6, [10, 0, 0, 2], [10, 0, 0, 1], tcp.len());
        let mut pkt = ethernet_hdr(ETHERTYPE_IPV4);
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&tcp);

        let result = decode(&pkt, LINKTYPE_ETHERNET, 0).unwrap();
        assert_eq!(result.direction, Direction::BtoA);
        // FlowKey normalizes: addr_a is the smaller
        assert_eq!(result.flow_key.addr_a.0, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn decode_non_tcp_returns_none() {
        let udp = vec![0u8; 8]; // minimal UDP
        let ip = ipv4_hdr(17, [10, 0, 0, 1], [10, 0, 0, 2], udp.len()); // protocol=17 UDP
        let mut pkt = ethernet_hdr(ETHERTYPE_IPV4);
        pkt.extend_from_slice(&ip);
        pkt.extend_from_slice(&udp);

        assert!(decode(&pkt, LINKTYPE_ETHERNET, 0).is_none());
    }

    #[test]
    fn decode_truncated_returns_none() {
        let data = [0u8; 5]; // way too short
        assert!(decode(&data, LINKTYPE_ETHERNET, 0).is_none());
    }

    #[test]
    fn decode_unsupported_link_type_returns_none() {
        let data = [0u8; 100];
        assert!(decode(&data, 999, 0).is_none());
    }
}
```

- [ ] **Step 2: Run all decoder tests**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-protocol de::`

Expected: All tests PASS (buf: 8, l2: 16, l3: 8, l4: 7, integration: 9 = 48 total).

- [ ] **Step 3: Commit**

```bash
git add server/ts-protocol/src/de/mod.rs
git commit -m "$(cat <<'EOF'
test(ts-protocol): add decode() integration tests

Full-stack tests: Ethernet/IPv4/TCP, IPv6/TCP, VLAN, Raw IP, SLL,
direction detection, non-TCP, truncated, unsupported link type.
EOF
)"
```

---

## Task 7: Wire up `de::decode()` and remove old parsing code

**Files:**
- Modify: `server/ts-protocol/src/flow.rs`
- Modify: `server/ts-protocol/src/net.rs`
- Modify: `server/ts-capture/src/packet.rs`

- [ ] **Step 1: Update `flow.rs` to use `de::decode()`**

Replace `server/ts-protocol/src/flow.rs` with:

```rust
use tokio::sync::mpsc;

use ts_common::internal_metrics::{Metric, MetricsWorker};

use crate::de;
use crate::net::ParsedPacket;
use ts_capture::RawPacket;

/// Parses raw packets and distributes them to workers by flow key hash.
pub struct FlowDispatcher {
    worker_txs: Vec<mpsc::Sender<ParsedPacket>>,
    metrics: MetricsWorker,
}

impl FlowDispatcher {
    pub fn new(worker_txs: Vec<mpsc::Sender<ParsedPacket>>, metrics: MetricsWorker) -> Self {
        Self { worker_txs, metrics }
    }

    /// Parse and dispatch a single raw packet.
    /// Returns false if all worker channels are closed.
    pub async fn dispatch(&self, raw: &RawPacket) -> bool {
        let parsed = match de::decode(&raw.data, raw.link_type, raw.timestamp_us) {
            Some(p) => p,
            None => return true, // Non-TCP or unparseable packet, skip.
        };

        self.metrics.counter(Metric::PipelinePacketsDispatched).inc();

        let worker_idx = (parsed.flow_key.shard_hash() as usize) % self.worker_txs.len();
        self.worker_txs[worker_idx].send(parsed).await.is_ok()
    }
}
```

- [ ] **Step 2: Remove old parsing functions from `net.rs`**

In `server/ts-protocol/src/net.rs`:

Remove the `use ts_capture::RawPacket;` import (line 6).

Remove the entire `parse_packet()` function and the three helper functions `parse_ipv4_tcp()`, `parse_ipv6_tcp()`, `parse_tcp()` (lines 102-200).

Keep everything else: `FlowKey`, `Direction`, `ParsedPacket`, TCP flag constants, `ParsedPacket` impl, and the `#[cfg(test)] mod tests` block.

The file should end up as:

```rust
use std::hash::{Hash, Hasher};
use std::net::IpAddr;

use bytes::Bytes;

/// Identifies a TCP connection. Normalized so the smaller (IP, port) pair is
/// always `addr_a`, ensuring both directions hash to the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowKey {
    pub addr_a: (IpAddr, u16),
    pub addr_b: (IpAddr, u16),
}

impl FlowKey {
    pub fn new(src_ip: IpAddr, src_port: u16, dst_ip: IpAddr, dst_port: u16) -> Self {
        let a = (src_ip, src_port);
        let b = (dst_ip, dst_port);
        if (src_ip, src_port) <= (dst_ip, dst_port) {
            Self { addr_a: a, addr_b: b }
        } else {
            Self { addr_a: b, addr_b: a }
        }
    }

    pub fn shard_hash(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl Hash for FlowKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.addr_a.0.hash(state);
        self.addr_a.1.hash(state);
        self.addr_b.0.hash(state);
        self.addr_b.1.hash(state);
    }
}

impl std::fmt::Display for FlowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{} <-> {}:{}",
            self.addr_a.0, self.addr_a.1, self.addr_b.0, self.addr_b.1
        )
    }
}

/// Which direction the packet is going relative to the normalized FlowKey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    AtoB,
    BtoA,
}

/// A parsed network packet with extracted TCP fields.
#[derive(Debug, Clone)]
pub struct ParsedPacket {
    pub flow_key: FlowKey,
    pub direction: Direction,
    pub src_ip: IpAddr,
    pub src_port: u16,
    pub dst_ip: IpAddr,
    pub dst_port: u16,
    pub tcp_flags: u8,
    pub tcp_seq: u32,
    pub tcp_ack: u32,
    pub payload: Bytes,
    pub timestamp_us: i64,
}

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;

impl ParsedPacket {
    pub fn has_syn(&self) -> bool {
        self.tcp_flags & TCP_SYN != 0
    }
    pub fn has_fin(&self) -> bool {
        self.tcp_flags & TCP_FIN != 0
    }
    pub fn has_rst(&self) -> bool {
        self.tcp_flags & TCP_RST != 0
    }
    pub fn has_ack(&self) -> bool {
        self.tcp_flags & TCP_ACK != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flow_key_normalization() {
        let ip_a: IpAddr = "10.0.0.1".parse().unwrap();
        let ip_b: IpAddr = "10.0.0.2".parse().unwrap();

        let fk1 = FlowKey::new(ip_a, 1000, ip_b, 80);
        let fk2 = FlowKey::new(ip_b, 80, ip_a, 1000);

        assert_eq!(fk1, fk2);
        assert_eq!(fk1.shard_hash(), fk2.shard_hash());
    }

    #[test]
    fn test_direction() {
        let ip_a: IpAddr = "10.0.0.1".parse().unwrap();
        let ip_b: IpAddr = "10.0.0.2".parse().unwrap();

        let fk = FlowKey::new(ip_a, 1000, ip_b, 80);
        assert_eq!(fk.addr_a, (ip_a, 1000));
        assert_eq!(fk.addr_b, (ip_b, 80));
    }
}
```

- [ ] **Step 3: Strip `ts-capture/packet.rs` to pure data struct**

Replace `server/ts-capture/src/packet.rs` with:

```rust
use bytes::Bytes;

/// A raw captured packet before any protocol parsing.
#[derive(Debug, Clone)]
pub struct RawPacket {
    /// Capture timestamp in microseconds since Unix epoch.
    pub timestamp_us: i64,
    /// Number of bytes actually captured.
    pub caplen: u32,
    /// Original length on the wire.
    pub wirelen: u32,
    /// Link type from the pcap header (e.g., 1 = Ethernet, 101 = Raw IP).
    pub link_type: u32,
    /// Raw packet data starting at the link layer.
    pub data: Bytes,
}
```

- [ ] **Step 4: Check if `ts-capture` dep can be removed from `ts-protocol`**

Run: `grep -r "ts_capture\|ts-capture" server/ts-protocol/src/`

Expected: Only `flow.rs` and `pipeline.rs` still import `ts_capture::RawPacket` (for the `dispatch` and `start_pipeline` signatures). The dependency stays — we still need `RawPacket` as the input envelope type.

- [ ] **Step 5: Verify all tests pass**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-capture -p ts-protocol`

Expected: All ts-capture tests pass (the `ip_offset` tests are gone). All ts-protocol tests pass (FlowKey tests + all new decoder tests).

- [ ] **Step 6: Verify the full workspace compiles**

Run: `cargo check --manifest-path server/Cargo.toml`

Expected: Clean compilation. No warnings about unused `parse_packet`.

- [ ] **Step 7: Commit**

```bash
git add server/ts-protocol/src/flow.rs server/ts-protocol/src/net.rs server/ts-capture/src/packet.rs
git commit -m "$(cat <<'EOF'
refactor(ts-protocol): wire de::decode(), remove parse_packet() and ip_offset()

FlowDispatcher now calls de::decode() instead of net::parse_packet().
Removed parse_packet/parse_ipv4_tcp/parse_ipv6_tcp/parse_tcp from net.rs.
Removed ip_offset/ethernet_ip_offset and all link-type constants from
ts-capture::RawPacket — it is now a pure data envelope.
EOF
)"
```

---

## Task 8: Run full test suite and verify no regressions

**Files:** None (verification only)

- [ ] **Step 1: Run all workspace tests**

Run: `cargo test --manifest-path server/Cargo.toml`

Expected: All tests pass across all crates (ts-capture, ts-protocol, ts-llm, tokenscope).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --manifest-path server/Cargo.toml -- -D warnings`

Expected: No warnings.

- [ ] **Step 3: If any test fails, diagnose and fix**

Check error output. Most likely causes:
- A downstream crate that was importing `parse_packet` or `ip_offset` — search for the symbol and update.
- A test that was constructing `RawPacket` and calling `ip_offset()` — remove or rewrite using `de::decode()` directly.

- [ ] **Step 4: Final commit if any fixes were needed**

```bash
git add -u
git commit -m "fix(ts-protocol): address test/clippy issues from decoder refactor"
```
