# Cloud-Probe Capture Source Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the `cloud-probe` capture source — a ZMQ `PULL` listener that receives batched packets from remote cloud-probe instances and feeds them into the existing TokenScope pipeline, with MPLS stripping added to the L2 decoder so the packets decode cleanly.

**Architecture:** New file `ts-capture/src/cloud_probe.rs` implementing `CaptureSource` using the `zeromq` pure-Rust crate (PULL bind, tokio-native). A pure `parse_batch` function handles the wire format. `ts-protocol`'s L2 decoder gets a new `strip_mpls` branch so cloud-probe's `Eth[+VLAN]+MPLS(4B)+IP` frames decode correctly. Batch metadata (uuid, service_tag) is discarded. On parse failure the whole batch is dropped.

**Tech Stack:** Rust, tokio, `zeromq` crate (v0.4, pure-Rust zmq.rs), `bytes`, `thiserror`, `tracing`.

**Spec:** `docs/superpowers/specs/2026-04-13-cloud-probe-capture-source-design.md`

---

## File Structure

| Path | Action | Responsibility |
|---|---|---|
| `server/ts-common/src/internal_metrics.rs` | Modify | Add `CaptureBatchesDropped` counter |
| `server/ts-common/src/config.rs` | Modify | Add `recv_hwm` field to `CloudProbe` variant |
| `server/ts-protocol/src/de/headers.rs` | Modify | Add `ETHERTYPE_MPLS` + `MplsHeader` |
| `server/ts-protocol/src/de/l2.rs` | Modify | Add `strip_mpls`, dispatch from `strip_vlan` |
| `server/Cargo.toml` | Modify | Add `zeromq` workspace dependency |
| `server/ts-capture/Cargo.toml` | Modify | Pull in `zeromq` |
| `server/ts-capture/src/lib.rs` | Modify | Register `cloud_probe` module, add `CaptureError::Zmq`, re-export |
| `server/ts-capture/src/cloud_probe.rs` | Create | `CloudProbeSource` + `parse_batch` + tests |
| `server/ts-capture/src/factory.rs` | Modify | Wire `CloudProbe` config to `CloudProbeSource` |
| `server/config/default.toml` | Modify | Update commented cloud-probe example with `recv_hwm` |
| `docs/design/capture.md` | Modify | Document recv_hwm, skipped version validation, MPLS delegation |

---

## Task 1: Add `CaptureBatchesDropped` internal metric

**Files:**
- Modify: `server/ts-common/src/internal_metrics.rs`

- [ ] **Step 1: Extend the capture-group metric list**

Open `server/ts-common/src/internal_metrics.rs` and locate the `define_metrics!` invocation (around line 124). Extend the `-- Capture --` block:

```rust
define_metrics! {
    // -- Capture --
    CapturePacketsReceived  => { kind: Counter, group: Capture,  short: "pkts_recv"       },
    CapturePacketsDropped   => { kind: Counter, group: Capture,  short: "pkts_drop"       },
    CaptureBatchesReceived  => { kind: Counter, group: Capture,  short: "batches_recv"    },
    CaptureBatchesDropped   => { kind: Counter, group: Capture,  short: "batches_drop"    },
    // ...unchanged below
```

- [ ] **Step 2: Verify compilation**

Run: `cd server && cargo check -p ts-common`
Expected: successful build, no warnings about the new variant.

- [ ] **Step 3: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-common/src/internal_metrics.rs
git commit -m "feat(ts-common): add CaptureBatchesDropped counter"
```

---

## Task 2: Add MPLS constant and header struct

**Files:**
- Modify: `server/ts-protocol/src/de/headers.rs`

- [ ] **Step 1: Add the MPLS constant and struct**

Append to the EtherType block in `server/ts-protocol/src/de/headers.rs` (after the existing `ETHERTYPE_QINQ`):

```rust
pub const ETHERTYPE_MPLS: u16 = 0x8847;
```

And add a new header block after the VLAN block:

```rust
// ---------------------------------------------------------------------------
// MPLS shim (4 bytes): 20-bit label, 3-bit TC, 1-bit S (bottom-of-stack), 8-bit TTL.
// Network byte order. We only need the S bit to know when to stop unwinding
// the label stack.
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct MplsHeader {
    pub bytes: [u8; 4],
}

impl MplsHeader {
    /// True when this label is the bottom-of-stack (S bit set).
    ///
    /// In the 32-bit label entry, the S bit is the LSB of the third byte.
    #[inline]
    pub fn bottom_of_stack(&self) -> bool {
        self.bytes[2] & 0x01 != 0
    }
}
```

- [ ] **Step 2: Verify compilation**

Run: `cd server && cargo check -p ts-protocol`
Expected: successful build.

- [ ] **Step 3: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-protocol/src/de/headers.rs
git commit -m "feat(ts-protocol): add MPLS ethertype and header struct"
```

---

## Task 3: MPLS stripping in L2 decoder (TDD)

**Files:**
- Modify: `server/ts-protocol/src/de/l2.rs`

- [ ] **Step 1: Write failing tests for MPLS stripping**

Append the following tests to the `mod tests` block in `server/ts-protocol/src/de/l2.rs` (before the closing `}`):

```rust
    // --- MPLS ---

    /// Build an MPLS label shim. `bottom=true` sets the S bit.
    fn mpls_label(bottom: bool) -> [u8; 4] {
        let mut b = [0u8; 4];
        if bottom {
            b[2] |= 0x01;
        }
        b
    }

    #[test]
    fn ethernet_mpls_ipv4() {
        // 12 zero MACs + 0x8847 (MPLS) + 4-byte label (S=1) + IPv4 version nibble
        let mut data = [0u8; 19];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(true));
        data[18] = 0x45; // IPv4, IHL=5
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 18);
    }

    #[test]
    fn ethernet_mpls_ipv6() {
        let mut data = [0u8; 19];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(true));
        data[18] = 0x60; // IPv6
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV6));
    }

    #[test]
    fn ethernet_vlan_mpls_ipv4() {
        // Eth + VLAN(tpid=0x8100, inner=0x8847) + MPLS(S=1) + IPv4
        let mut data = [0u8; 23];
        data[12] = 0x81;
        data[13] = 0x00;
        // TCI at 14-15 (zero)
        data[16] = 0x88;
        data[17] = 0x47;
        data[18..22].copy_from_slice(&mpls_label(true));
        data[22] = 0x45;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 22);
    }

    #[test]
    fn ethernet_mpls_multilabel_ipv4() {
        // Two MPLS labels (first with S=0, second with S=1) then IPv4
        let mut data = [0u8; 23];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(false));
        data[18..22].copy_from_slice(&mpls_label(true));
        data[22] = 0x45;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Ok(ETHERTYPE_IPV4));
        assert_eq!(buf.offset(), 22);
    }

    #[test]
    fn ethernet_mpls_non_ip_returns_not_ip() {
        // Bottom label followed by a byte whose top nibble is neither 4 nor 6
        let mut data = [0u8; 19];
        data[12] = 0x88;
        data[13] = 0x47;
        data[14..18].copy_from_slice(&mpls_label(true));
        data[18] = 0x00;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Err(DecodeError::NotIp));
    }

    #[test]
    fn ethernet_mpls_truncated_shim() {
        // Eth + MPLS ether_type but only 2 bytes of shim present
        let mut data = [0u8; 16];
        data[12] = 0x88;
        data[13] = 0x47;
        let mut buf = PacketBuf::new(&data);
        assert_eq!(decode_l2(&mut buf, LINKTYPE_ETHERNET), Err(DecodeError::Truncated));
    }
```

- [ ] **Step 2: Run tests — expect failures**

Run: `cd server && cargo test -p ts-protocol --lib de::l2`
Expected: The six new tests fail to compile (MPLS constants/handling not yet wired). If compilation passes, the new tests should fail on the MPLS assertions because `strip_vlan` returns `Ok(0x8847)` today, not `ETHERTYPE_IPV4`.

- [ ] **Step 3: Implement `strip_mpls` and wire it from `strip_vlan`**

Replace the body of `server/ts-protocol/src/de/l2.rs` from the `use` block through `strip_vlan` with the new implementation below. Keep everything else in the file unchanged.

```rust
use super::buf::PacketBuf;
use super::error::{DecodeError, DecodeResult};
use super::headers::*;
use super::try_consume;

/// Decode the L2 header for the given `link_type` and advance the buffer past
/// it. Returns the EtherType (or equivalent protocol number) that identifies
/// the L3 payload.
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
    let hdr = try_consume!(buf, EthernetHeader);
    let ether_type = hdr.ether_type();
    resolve_next(buf, ether_type)
}

/// Recursively strip VLAN/QinQ tags and MPLS label stacks, returning the
/// final EtherType (or IP pseudo-EtherType) that identifies the L3 payload.
fn resolve_next(buf: &mut PacketBuf, ether_type: u16) -> DecodeResult<u16> {
    match ether_type {
        ETHERTYPE_VLAN | ETHERTYPE_QINQ => {
            let vlan = try_consume!(buf, VlanHeader);
            resolve_next(buf, vlan.ether_type())
        }
        ETHERTYPE_MPLS => strip_mpls(buf),
        other => Ok(other),
    }
}

/// Consume MPLS label shims until the bottom-of-stack is reached, then peek
/// the first nibble of the next byte to distinguish IPv4 from IPv6.
fn strip_mpls(buf: &mut PacketBuf) -> DecodeResult<u16> {
    loop {
        let label = try_consume!(buf, MplsHeader);
        if label.bottom_of_stack() {
            break;
        }
    }
    let byte = buf.peek::<u8>().ok_or(DecodeError::Truncated)?;
    match byte >> 4 {
        4 => Ok(ETHERTYPE_IPV4),
        6 => Ok(ETHERTYPE_IPV6),
        _ => Err(DecodeError::NotIp),
    }
}
```

Then delete the now-unused `strip_vlan` helper — it has been folded into `resolve_next`.

- [ ] **Step 4: Run all L2 tests**

Run: `cd server && cargo test -p ts-protocol --lib de::l2`
Expected: all existing VLAN/QinQ tests still pass (they now go through `resolve_next`), and the six new MPLS tests pass.

- [ ] **Step 5: Run the full ts-protocol test suite**

Run: `cd server && cargo test -p ts-protocol`
Expected: all tests pass, including the end-to-end decode tests in `de/mod.rs`.

- [ ] **Step 6: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-protocol/src/de/l2.rs
git commit -m "feat(ts-protocol): strip MPLS label stacks in L2 decoder"
```

---

## Task 4: Add `recv_hwm` to the CloudProbe config variant

**Files:**
- Modify: `server/ts-common/src/config.rs`

- [ ] **Step 1: Extend `CaptureSourceConfig::CloudProbe`**

In `server/ts-common/src/config.rs`, replace the existing `CloudProbe` variant and the accompanying default helper. Before:

```rust
    CloudProbe {
        #[serde(default = "default_cloud_probe_endpoint")]
        endpoint: String,
    },
```

After:

```rust
    CloudProbe {
        #[serde(default = "default_cloud_probe_endpoint")]
        endpoint: String,
        #[serde(default = "default_cloud_probe_hwm")]
        recv_hwm: i32,
    },
```

And add next to `default_cloud_probe_endpoint`:

```rust
fn default_cloud_probe_hwm() -> i32 {
    1000
}
```

- [ ] **Step 2: Verify compilation**

Run: `cd server && cargo check -p ts-common`
Expected: compiles — the `ts-capture::factory::build_source` call site will produce a warning or error because it pattern-matches `CloudProbe { endpoint }` without the new field. Note the failure (it is fixed in Task 9).

If the build fails in `ts-capture`, run only the common crate for this step: `cargo check -p ts-common`. The fix for the `ts-capture` mismatch is part of Task 9.

- [ ] **Step 3: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-common/src/config.rs
git commit -m "feat(ts-common): add recv_hwm to cloud-probe config"
```

---

## Task 5: Add `zeromq` dependency

**Files:**
- Modify: `server/Cargo.toml`
- Modify: `server/ts-capture/Cargo.toml`

- [ ] **Step 1: Add to workspace dependencies**

In `server/Cargo.toml`, inside `[workspace.dependencies]`, add the line below next to `pcap`:

```toml
# Packet capture
pcap = "2"
zeromq = { version = "0.4", default-features = false, features = ["tokio-runtime", "tcp-transport"] }
bytes = "1"
async-trait = "0.1"
```

- [ ] **Step 2: Pull it into `ts-capture`**

In `server/ts-capture/Cargo.toml`, add `zeromq.workspace = true` in `[dependencies]`:

```toml
[dependencies]
ts-common.workspace = true
pcap.workspace = true
zeromq.workspace = true
bytes.workspace = true
async-trait.workspace = true
tokio.workspace = true
tracing.workspace = true
thiserror.workspace = true
tokio-util.workspace = true
```

- [ ] **Step 3: Download and verify the crate resolves**

Run: `cd server && cargo fetch`
Expected: `zeromq v0.4.x` fetched from crates.io.

Run: `cd server && cargo check -p ts-capture`
Expected: compiles (zeromq not yet used anywhere; the existing `CloudProbe { endpoint }` pattern match in `factory.rs` may error — that's fine, it is fixed in Task 9).

If `zeromq 0.4.x` is unavailable on crates.io, bump to the latest published major (e.g. `0.5`). Confirm by running `cargo search zeromq` and read the top result.

- [ ] **Step 4: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/Cargo.toml server/Cargo.lock server/ts-capture/Cargo.toml
git commit -m "chore(deps): add zeromq pure-Rust crate for cloud-probe source"
```

---

## Task 6: Add `CaptureError::Zmq` variant

**Files:**
- Modify: `server/ts-capture/src/lib.rs`

- [ ] **Step 1: Add the new error variant**

In `server/ts-capture/src/lib.rs`, extend the `CaptureError` enum:

```rust
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("pcap error: {0}")]
    Pcap(#[from] pcap::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("zmq error: {0}")]
    Zmq(#[from] zeromq::ZmqError),

    #[error("{0}")]
    Other(String),
}
```

- [ ] **Step 2: Verify compilation**

Run: `cd server && cargo check -p ts-capture`
Expected: compiles (new variant is unused but valid).

- [ ] **Step 3: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/src/lib.rs
git commit -m "feat(ts-capture): add CaptureError::Zmq variant"
```

---

## Task 7: Implement `parse_batch` pure function (TDD)

**Files:**
- Create: `server/ts-capture/src/cloud_probe.rs`
- Modify: `server/ts-capture/src/lib.rs`

- [ ] **Step 1: Register the module**

In `server/ts-capture/src/lib.rs`, near the top where other modules are listed:

```rust
mod cloud_probe;
mod factory;
mod packet;
mod pcap_file;
mod pcap_live;
mod source;

pub use cloud_probe::CloudProbeSource;
pub use packet::RawPacket;
pub use factory::build_source;
pub use pcap_file::PcapFileSource;
pub use pcap_live::PcapLiveSource;
pub use source::CaptureSource;
```

- [ ] **Step 2: Create `cloud_probe.rs` with the parse_batch signature and failing tests**

Create `server/ts-capture/src/cloud_probe.rs` with this initial content — `parse_batch` is intentionally left unimplemented so tests fail:

```rust
//! Cloud-probe capture source.
//!
//! Receives batches of packets over a ZMQ `PULL` socket from remote
//! cloud-probe instances. Each batch carries a 24-byte header followed by
//! `pkts_num` per-packet records (wire format documented in
//! `docs/design/capture.md`). Batch-level metadata (uuid, service_tag,
//! keybit) is currently discarded — only the packet bytes and timestamps
//! propagate downstream.

use bytes::Bytes;
use thiserror::Error;

use crate::packet::RawPacket;

const BATCH_HDR_LEN: usize = 24;
const PKT_HDR_LEN: usize = 16;
const PKT_DATA_LEN_FIELD: usize = 2;

/// Link-type cloud-probe always uses at the Ethernet layer.
/// Matches `ts-protocol::de::headers::LINKTYPE_ETHERNET` (value 1).
const LINKTYPE_ETHERNET: u32 = 1;

/// Failure encountered while parsing a ZMQ batch payload. On any failure the
/// caller drops the entire batch (see design doc §Error Handling).
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum BatchError {
    #[error("batch truncated: needed {needed} more bytes at offset {offset}, have {have}")]
    Truncated {
        needed: usize,
        have: usize,
        offset: usize,
    },
}

/// Parse a single ZMQ batch message into a vector of RawPackets.
///
/// Does NOT validate the `version` field: per design decision we accept any
/// batch whose length arithmetic is self-consistent.
pub(crate) fn parse_batch(_bytes: &[u8]) -> Result<Vec<RawPacket>, BatchError> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a batch-header blob with the given packet count.
    fn batch_header(pkts_num: u16) -> Vec<u8> {
        let mut v = Vec::with_capacity(BATCH_HDR_LEN);
        v.extend_from_slice(&2u16.to_be_bytes()); // version
        v.extend_from_slice(&pkts_num.to_be_bytes()); // pkts_num
        v.extend_from_slice(&0u32.to_be_bytes()); // keybit
        v.extend_from_slice(&[0u8; 16]); // uuid
        v
    }

    /// Append one packet record with the given timestamp and payload bytes.
    /// `wirelen_extra` lets tests exercise caplen < wirelen.
    fn append_packet(
        buf: &mut Vec<u8>,
        tv_sec: u32,
        tv_usec: u32,
        payload: &[u8],
        wirelen_extra: u32,
    ) {
        let caplen = payload.len() as u32;
        let wirelen = caplen + wirelen_extra;
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(&tv_sec.to_be_bytes());
        buf.extend_from_slice(&tv_usec.to_be_bytes());
        buf.extend_from_slice(&caplen.to_be_bytes());
        buf.extend_from_slice(&wirelen.to_be_bytes());
        buf.extend_from_slice(payload);
    }

    #[test]
    fn zero_packets() {
        let bytes = batch_header(0);
        let pkts = parse_batch(&bytes).unwrap();
        assert!(pkts.is_empty());
    }

    #[test]
    fn single_packet_roundtrip() {
        let mut bytes = batch_header(1);
        append_packet(&mut bytes, 100, 250_000, &[0xaa, 0xbb, 0xcc, 0xdd], 0);
        let pkts = parse_batch(&bytes).unwrap();
        assert_eq!(pkts.len(), 1);
        let p = &pkts[0];
        assert_eq!(p.timestamp_us, 100 * 1_000_000 + 250_000);
        assert_eq!(p.caplen, 4);
        assert_eq!(p.wirelen, 4);
        assert_eq!(p.link_type, LINKTYPE_ETHERNET);
        assert_eq!(&p.data[..], &[0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn multiple_packets_preserve_order() {
        let mut bytes = batch_header(3);
        append_packet(&mut bytes, 1, 0, &[0x01], 0);
        append_packet(&mut bytes, 2, 0, &[0x02, 0x02], 0);
        append_packet(&mut bytes, 3, 0, &[0x03, 0x03, 0x03], 0);
        let pkts = parse_batch(&bytes).unwrap();
        assert_eq!(pkts.len(), 3);
        assert_eq!(&pkts[0].data[..], &[0x01]);
        assert_eq!(&pkts[1].data[..], &[0x02, 0x02]);
        assert_eq!(&pkts[2].data[..], &[0x03, 0x03, 0x03]);
    }

    #[test]
    fn caplen_can_be_less_than_wirelen() {
        let mut bytes = batch_header(1);
        append_packet(&mut bytes, 0, 0, &[0xff; 10], 40);
        let pkts = parse_batch(&bytes).unwrap();
        assert_eq!(pkts[0].caplen, 10);
        assert_eq!(pkts[0].wirelen, 50);
    }

    #[test]
    fn truncated_batch_header() {
        let bytes = vec![0u8; BATCH_HDR_LEN - 1];
        let err = parse_batch(&bytes).unwrap_err();
        assert!(matches!(err, BatchError::Truncated { .. }));
    }

    #[test]
    fn truncated_pkt_record_header() {
        let mut bytes = batch_header(1);
        // Only 5 of the 18 bytes required for pkt_data_len + pkt_hdr
        bytes.extend_from_slice(&[0u8; 5]);
        let err = parse_batch(&bytes).unwrap_err();
        assert!(matches!(err, BatchError::Truncated { .. }));
    }

    #[test]
    fn pkt_data_len_exceeds_buffer() {
        let mut bytes = batch_header(1);
        // Claim 100 bytes of pkt_data but supply only 10
        bytes.extend_from_slice(&100u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes()); // tv_sec
        bytes.extend_from_slice(&0u32.to_be_bytes()); // tv_usec
        bytes.extend_from_slice(&100u32.to_be_bytes()); // caplen
        bytes.extend_from_slice(&100u32.to_be_bytes()); // wirelen
        bytes.extend_from_slice(&[0u8; 10]);
        let err = parse_batch(&bytes).unwrap_err();
        assert!(matches!(err, BatchError::Truncated { .. }));
    }
}
```

- [ ] **Step 3: Run tests — expect unimplemented panic**

Run: `cd server && cargo test -p ts-capture --lib cloud_probe`
Expected: tests build, all six fail with a panic from `unimplemented!()`.

- [ ] **Step 4: Implement `parse_batch`**

Replace the `unimplemented!()` body with:

```rust
pub(crate) fn parse_batch(bytes: &[u8]) -> Result<Vec<RawPacket>, BatchError> {
    let total = bytes.len();
    let mut offset = 0usize;

    if total < BATCH_HDR_LEN {
        return Err(BatchError::Truncated {
            needed: BATCH_HDR_LEN - total,
            have: total,
            offset,
        });
    }

    // Only pkts_num is consumed; version/keybit/uuid are ignored.
    let pkts_num = u16::from_be_bytes([bytes[2], bytes[3]]);
    offset += BATCH_HDR_LEN;

    let mut out = Vec::with_capacity(pkts_num as usize);
    for _ in 0..pkts_num {
        let record_header_len = PKT_DATA_LEN_FIELD + PKT_HDR_LEN;
        if total - offset < record_header_len {
            return Err(BatchError::Truncated {
                needed: record_header_len - (total - offset),
                have: total - offset,
                offset,
            });
        }

        let pkt_data_len =
            u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += PKT_DATA_LEN_FIELD;

        let tv_sec = read_u32_be(bytes, offset);
        let tv_usec = read_u32_be(bytes, offset + 4);
        let caplen = read_u32_be(bytes, offset + 8);
        let wirelen = read_u32_be(bytes, offset + 12);
        offset += PKT_HDR_LEN;

        if total - offset < pkt_data_len {
            return Err(BatchError::Truncated {
                needed: pkt_data_len - (total - offset),
                have: total - offset,
                offset,
            });
        }

        let data = Bytes::copy_from_slice(&bytes[offset..offset + pkt_data_len]);
        offset += pkt_data_len;

        out.push(RawPacket {
            timestamp_us: tv_sec as i64 * 1_000_000 + tv_usec as i64,
            caplen,
            wirelen,
            link_type: LINKTYPE_ETHERNET,
            data,
        });
    }

    Ok(out)
}

#[inline]
fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
```

- [ ] **Step 5: Run tests — expect pass**

Run: `cd server && cargo test -p ts-capture --lib cloud_probe`
Expected: all six tests pass.

- [ ] **Step 6: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/src/cloud_probe.rs server/ts-capture/src/lib.rs
git commit -m "feat(ts-capture): add cloud-probe batch parser"
```

---

## Task 8: Implement `CloudProbeSource::run`

**Files:**
- Modify: `server/ts-capture/src/cloud_probe.rs`

- [ ] **Step 1: Add the struct, imports, and `CaptureSource` impl**

Below the `parse_batch` implementation (and above the `#[cfg(test)]` block) in `server/ts-capture/src/cloud_probe.rs`, add:

```rust
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeromq::{PullSocket, Socket, SocketOptions, SocketRecv, ZmqMessage};

use ts_common::internal_metrics::{Metric, MetricsWorker};

use crate::source::CaptureSource;

/// Throttle for malformed-batch warnings: at most one log every 5 seconds.
const WARN_THROTTLE: Duration = Duration::from_secs(5);

pub struct CloudProbeSource {
    endpoint: String,
    recv_hwm: i32,
}

impl CloudProbeSource {
    pub fn new(endpoint: String, recv_hwm: i32) -> Self {
        Self { endpoint, recv_hwm }
    }
}

#[async_trait]
impl CaptureSource for CloudProbeSource {
    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<RawPacket>,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        let endpoint = self.endpoint.clone();
        let recv_hwm = self.recv_hwm;

        let mut opts = SocketOptions::default();
        opts.set_rcvhwm(recv_hwm);
        let mut socket = PullSocket::with_options(opts);

        socket.bind(&endpoint).await?;
        tracing::info!(
            "cloud-probe: listening on {} (recv_hwm={})",
            endpoint,
            recv_hwm,
        );

        let mut batch_count: u64 = 0;
        let mut pkt_count: u64 = 0;
        let mut last_warn: Option<Instant> = None;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::debug!("cloud-probe: cancellation requested, stopping");
                    break;
                }
                msg = socket.recv() => {
                    match msg {
                        Ok(msg) => {
                            let bytes = flatten_message(&msg);
                            match parse_batch(&bytes) {
                                Ok(pkts) => {
                                    metrics.counter(Metric::CaptureBatchesReceived).inc();
                                    batch_count += 1;
                                    for pkt in pkts {
                                        if tx.send(pkt).await.is_err() {
                                            tracing::debug!(
                                                "cloud-probe: channel closed, stopping"
                                            );
                                            log_summary(&endpoint, batch_count, pkt_count);
                                            return Ok(());
                                        }
                                        metrics
                                            .counter(Metric::CapturePacketsReceived)
                                            .inc();
                                        pkt_count += 1;
                                    }
                                }
                                Err(err) => {
                                    metrics.counter(Metric::CaptureBatchesDropped).inc();
                                    let now = Instant::now();
                                    let should_warn = last_warn
                                        .map(|t| now.duration_since(t) >= WARN_THROTTLE)
                                        .unwrap_or(true);
                                    if should_warn {
                                        tracing::warn!(
                                            "cloud-probe: dropping malformed batch ({} bytes): {}",
                                            bytes.len(),
                                            err,
                                        );
                                        last_warn = Some(now);
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            tracing::error!("cloud-probe: recv error: {err}");
                            log_summary(&endpoint, batch_count, pkt_count);
                            return Err(err.into());
                        }
                    }
                }
            }
        }

        log_summary(&endpoint, batch_count, pkt_count);
        Ok(())
    }
}

/// Concatenate the frames of a ZMQ message. cloud-probe always sends a
/// single-frame batch, but zmq.rs surfaces the message as a sequence of
/// `Bytes` frames, so we flatten defensively.
fn flatten_message(msg: &ZmqMessage) -> Vec<u8> {
    let mut v = Vec::with_capacity(msg.iter().map(|b| b.len()).sum());
    for frame in msg.iter() {
        v.extend_from_slice(frame);
    }
    v
}

fn log_summary(endpoint: &str, batches: u64, packets: u64) {
    tracing::info!(
        "cloud-probe: stopped {} (batches={}, packets={})",
        endpoint,
        batches,
        packets,
    );
}
```

- [ ] **Step 2: Verify it builds**

Run: `cd server && cargo check -p ts-capture`
Expected: build succeeds. The `zeromq` API surface used above (`PullSocket::with_options`, `SocketOptions::default`, `set_rcvhwm`, `Socket::bind`, `SocketRecv::recv`, `ZmqMessage::get/iter/len`) is what the `zeromq = "0.4"` crate exposes. If any name differs in the actual crate version, adjust imports to match — note the actual APIs used in `cargo doc --open -p zeromq` (or `cargo tree -p zeromq` + the crate's source on crates.io). The shape of the change is: create socket with desired HWM → `bind` → loop `recv`.

- [ ] **Step 3: Run all ts-capture tests**

Run: `cd server && cargo test -p ts-capture`
Expected: existing parse_batch tests still pass; no new tests were added in this task (the end-to-end run loop is covered in Task 11 with a live socket).

- [ ] **Step 4: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/src/cloud_probe.rs
git commit -m "feat(ts-capture): implement CloudProbeSource run loop"
```

---

## Task 9: Wire factory to CloudProbeSource

**Files:**
- Modify: `server/ts-capture/src/factory.rs`

- [ ] **Step 1: Replace the error branch and update tests**

Overwrite `server/ts-capture/src/factory.rs` with:

```rust
use ts_common::config::CaptureSourceConfig;

use crate::cloud_probe::CloudProbeSource;
use crate::pcap_file::PcapFileSource;
use crate::pcap_live::PcapLiveSource;
use crate::source::CaptureSource;

/// Build a [`CaptureSource`] from configuration.
pub fn build_source(config: &CaptureSourceConfig) -> crate::Result<Box<dyn CaptureSource>> {
    match config {
        CaptureSourceConfig::Pcap {
            interface,
            bpf_filter,
            snaplen,
        } => Ok(Box::new(PcapLiveSource::new(
            interface.clone(),
            bpf_filter.clone(),
            *snaplen,
        ))),
        CaptureSourceConfig::PcapFile { path, .. } => {
            Ok(Box::new(PcapFileSource::new(path.into())))
        }
        CaptureSourceConfig::CloudProbe {
            endpoint,
            recv_hwm,
        } => Ok(Box::new(CloudProbeSource::new(endpoint.clone(), *recv_hwm))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_pcap_file_source() {
        let config = CaptureSourceConfig::PcapFile {
            path: "/tmp/test.pcap".to_string(),
            realtime: false,
        };
        assert!(build_source(&config).is_ok());
    }

    #[test]
    fn test_build_pcap_live_source() {
        let config = CaptureSourceConfig::Pcap {
            interface: "lo0".to_string(),
            bpf_filter: None,
            snaplen: 65535,
        };
        assert!(build_source(&config).is_ok());
    }

    #[test]
    fn test_build_cloud_probe_source() {
        let config = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
            recv_hwm: 1000,
        };
        assert!(build_source(&config).is_ok());
    }
}
```

- [ ] **Step 2: Run the factory tests**

Run: `cd server && cargo test -p ts-capture --lib factory`
Expected: all three tests pass.

- [ ] **Step 3: Build the full workspace**

Run: `cd server && cargo build`
Expected: builds cleanly. `ts-common`, `ts-protocol`, `ts-capture`, and the app binary all compile with the new config field wired through.

- [ ] **Step 4: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/src/factory.rs
git commit -m "feat(ts-capture): wire CloudProbe config to CloudProbeSource"
```

---

## Task 10: Update example config and design doc

**Files:**
- Modify: `server/config/default.toml`
- Modify: `docs/design/capture.md`

- [ ] **Step 1: Update the commented example block**

In `server/config/default.toml`, replace the cloud-probe example:

```toml
# [[capture.sources]]
# type = "cloud-probe"
# endpoint = "tcp://0.0.0.0:5555"
# recv_hwm = 1000
```

- [ ] **Step 2: Update the capture design doc**

In `docs/design/capture.md`, edit the cloud-probe section (currently lines 28–58). Replace the existing "Cloud-Probe — Remote Packet Ingestion via ZMQ" paragraph and the wire-format block with:

```markdown
### 3. Cloud-Probe — Remote Packet Ingestion via ZMQ

Receives batched packets from [cloud-probe](https://github.com/Netis/cloud-probe) instances deployed on remote servers.

- Rust crate: `zeromq` ([zmq.rs](https://github.com/zeromq/zmq.rs)) — pure Rust ZMQ implementation, native Tokio async, no libzmq dependency
- Uses ZMQ `PULL` socket bound to the configured endpoint (cloud-probe sends via `PUSH` and connects to us)
- `recv_hwm` is configurable (default 1000). When the downstream mpsc channel backpressures, the recv loop awaits and ZMQ's HWM eventually causes the probe side to drop — matches where the cloud-probe drop statistics already live.
- Runs as a Tokio task (native async, no spawn_blocking needed)
- Extracts individual packets from the ZMQ batch format, outputs raw `pkt_data` bytes as-is (including Ethernet + VLAN + MPLS headers)
- The `version` field in the batch header is **not** validated; any parseable batch is accepted. A malformed batch (length arithmetic inconsistent) is dropped whole and counted in the `CaptureBatchesDropped` internal metric.
- Batch-level metadata (`uuid`, `service_tag`, `keybit`) is currently discarded. If per-probe attribution becomes a requirement, add fields to `RawPacket` and plumb downstream.

#### Cloud-Probe ZMQ Wire Format

All multi-byte fields are **network byte order (big-endian)**.

\`\`\`
ZMQ Message (one batch per zmq_send):

┌──────────────── Batch Header (24 bytes) ────────────────┐
│ version: u16 │ pkts_num: u16 │ keybit: u32 │ uuid: [u8; 16] │
└─────────────────────────────────────────────────────────┘

Repeated pkts_num times:
┌──────────────── Per-Packet ─────────────────────────────┐
│ pkt_data_len: u16  (total length including MPLS header) │
│ pkt_hdr (16 bytes):                                     │
│   tv_sec: u32, tv_usec: u32, caplen: u32, len: u32      │
│ pkt_data: [u8; pkt_data_len]                            │
│   = Ethernet + [VLAN] + MPLS(4B) + IP payload           │
└─────────────────────────────────────────────────────────┘
\`\`\`

**MPLS Header (4 bytes):** Injected by cloud-probe into the Ethernet frame with ether_type `0x8847`. Stripping happens in `ts-protocol`'s L2 decoder, which unwinds the label stack until it finds the bottom-of-stack bit and then peeks the next nibble to detect IPv4 vs IPv6.
```

(Replace the literal backtick fences in the doc — they're escaped above to keep this plan block intact.)

Then find the "Configuration" block and change the cloud-probe TOML example to include `recv_hwm = 1000`.

- [ ] **Step 3: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/config/default.toml docs/design/capture.md
git commit -m "docs(capture): document cloud-probe recv_hwm and design notes"
```

---

## Task 11: End-to-end integration test

**Files:**
- Modify: `server/ts-capture/src/cloud_probe.rs` (append to `mod tests`)

- [ ] **Step 1: Add an integration test that drives a real PUSH→PULL exchange**

Append to the `#[cfg(test)] mod tests` block in `server/ts-capture/src/cloud_probe.rs`:

```rust
    use std::net::TcpListener;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use zeromq::{PushSocket, Socket, SocketOptions, SocketSend, ZmqMessage};

    use ts_common::internal_metrics::{Metric, MetricsSystem};

    fn test_metrics() -> ts_common::internal_metrics::MetricsWorker {
        let mut sys = MetricsSystem::new();
        sys.register_worker(
            "test",
            &[
                Metric::CapturePacketsReceived,
                Metric::CapturePacketsDropped,
                Metric::CaptureBatchesReceived,
                Metric::CaptureBatchesDropped,
            ],
        )
    }

    /// Reserve a free localhost port by opening then immediately dropping a
    /// TCP listener. The kernel won't hand the port to another process in
    /// the brief window before we bind the ZMQ socket.
    fn pick_free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    fn build_sample_batch(packets: &[(u32, u32, &[u8])]) -> Vec<u8> {
        let mut v = batch_header(packets.len() as u16);
        for (tv_sec, tv_usec, payload) in packets {
            append_packet(&mut v, *tv_sec, *tv_usec, payload, 0);
        }
        v
    }

    #[tokio::test]
    async fn integration_receives_packets_from_push_socket() {
        let port = pick_free_port();
        let endpoint = format!("tcp://127.0.0.1:{port}");

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();

        let source = Box::new(CloudProbeSource::new(endpoint.clone(), 100));
        let metrics = test_metrics();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            source.run(tx, metrics, cancel_clone).await
        });

        // Give the PULL socket a moment to bind before we connect.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut pusher = PushSocket::with_options(SocketOptions::default());
        pusher.connect(&endpoint).await.unwrap();

        let batch = build_sample_batch(&[
            (100, 123_456, &[0xaa, 0xbb][..]),
            (101, 0, &[0xcc][..]),
        ]);
        pusher.send(ZmqMessage::from(batch)).await.unwrap();

        // Collect both packets (allow some time for ZMQ to deliver).
        let pkt1 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for first packet")
            .expect("channel closed");
        let pkt2 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for second packet")
            .expect("channel closed");

        assert_eq!(pkt1.timestamp_us, 100 * 1_000_000 + 123_456);
        assert_eq!(&pkt1.data[..], &[0xaa, 0xbb]);
        assert_eq!(pkt2.timestamp_us, 101 * 1_000_000);
        assert_eq!(&pkt2.data[..], &[0xcc]);

        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("source task did not exit")
            .expect("join error");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn integration_malformed_batch_is_dropped() {
        let port = pick_free_port();
        let endpoint = format!("tcp://127.0.0.1:{port}");

        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();

        let source = Box::new(CloudProbeSource::new(endpoint.clone(), 100));
        let metrics = test_metrics();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            source.run(tx, metrics, cancel_clone).await
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut pusher = PushSocket::with_options(SocketOptions::default());
        pusher.connect(&endpoint).await.unwrap();

        // Send a payload shorter than the batch header
        pusher
            .send(ZmqMessage::from(vec![0u8; 10]))
            .await
            .unwrap();

        // Then send a well-formed batch with one packet so we can assert the
        // loop survived the malformed one.
        let good = build_sample_batch(&[(1, 0, &[0x01][..])]);
        pusher.send(ZmqMessage::from(good)).await.unwrap();

        let pkt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for packet after malformed batch")
            .expect("channel closed");
        assert_eq!(&pkt.data[..], &[0x01]);

        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("source task did not exit")
            .expect("join error");
        assert!(result.is_ok());
    }
```

- [ ] **Step 2: Run the integration tests**

Run: `cd server && cargo test -p ts-capture --lib cloud_probe::tests::integration`
Expected: both tests pass. If `zeromq` API names differ (e.g. `PushSocket::connect` vs `PushSocket::new().connect()`), adjust imports — the intent is: create PUSH socket, connect to the PULL endpoint, send a ZmqMessage, await downstream RawPacket.

- [ ] **Step 3: Run the full workspace test suite**

Run: `cd server && cargo test`
Expected: all existing tests still pass; no flaky timing (the 100ms pre-bind sleep is conservative).

- [ ] **Step 4: Commit**

```bash
cd /Users/timmy/code/netis/TokenScope
git add server/ts-capture/src/cloud_probe.rs
git commit -m "test(ts-capture): end-to-end cloud-probe PUSH→PULL integration tests"
```

---

## Verification Checklist (final pass)

- [ ] `cd server && cargo build` succeeds with all three source types enabled.
- [ ] `cd server && cargo test` green.
- [ ] `cd server && cargo clippy --all-targets -- -D warnings` clean (or no new warnings vs main).
- [ ] Starting the binary with a cloud-probe source configured logs `cloud-probe: listening on tcp://… (recv_hwm=…)`.
- [ ] Sending a real batch from a running cloud-probe (or the provided `recvzmq` reference in reverse — a manual test against `cpworker` if available) produces populated `llm_calls` rows in DuckDB. (Optional acceptance step; integration tests already cover the plumbing.)
