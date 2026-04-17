# ts-capture 重构实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 ts-capture 从"单 source pcap MVP"升级为正确、可测试、可扩展的统一采集层。

**Architecture:** 按评审优先级分5个任务：(1) 修复 PcapFileSource 错误吞没 bug，(2) 强化链路层解析支持 VLAN/Linux cooked capture，(3) 统一 source 构造逻辑让 config 与 trait 闭环，(4) 增加显式 shutdown 机制，(5) 补齐行为测试。每个任务都 TDD 先行。

**Tech Stack:** Rust, pcap 2.x, tokio, bytes, async-trait

**Review doc:** `docs/review/ts-capture-review.md`

---

### Task 1: 修复 PcapFileSource 错误吞没 bug（高优先级）

**Files:**
- Modify: `server/ts-capture/src/pcap_file.rs:46`
- Test: `server/ts-capture/src/pcap_file.rs` (inline tests)

当前 `while let Ok(packet) = cap.next_packet()` 会把所有错误（包括读取失败、文件损坏）都当成正常 EOF 退出。需要改为显式 match，仅 `NoMorePackets` 正常退出，其他错误上报。

- [ ] **Step 1: Write the failing test**

在 `server/ts-capture/src/pcap_file.rs` 文件末尾添加测试模块。由于我们无法直接构造一个损坏的 pcap 文件来触发非 EOF 错误，我们先用一个不存在的文件路径来验证错误传播，同时也写一个正常 pcap 文件的读取测试。

创建测试用 pcap 文件：先确认项目中是否已有测试用 pcap 文件。如果没有，在 `server/ts-capture/tests/fixtures/` 下放置一个最小的有效 pcap 文件（可以用 `tcpdump -c 1 -w` 生成）和一个截断的损坏文件。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_pcap_file_not_found_returns_error() {
        let source = PcapFileSource::new(PathBuf::from("/nonexistent/test.pcap"));
        let (tx, _rx) = mpsc::channel(16);
        let metrics = MetricsWorker::noop();
        let result = Box::new(source).run(tx, metrics).await;
        assert!(result.is_err(), "should return error for missing file");
    }
}
```

注意：`MetricsWorker::noop()` 可能尚不存在，需要在 ts-common 中添加（见 Step 3）。

- [ ] **Step 2: Add `MetricsWorker::noop()` test helper**

在 `server/ts-common/src/internal_metrics.rs` 中添加：

```rust
impl MetricsWorker {
    /// Create a no-op MetricsWorker for testing.
    #[cfg(any(test, feature = "test-util"))]
    pub fn noop() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self { tx }
    }
}
```

如果 `MetricsWorker` 的内部结构不同，根据实际字段调整。关键是提供一个可在测试中使用的空操作实例。

- [ ] **Step 3: Run test to verify it fails or passes the error propagation path**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-capture -- test_pcap_file_not_found_returns_error -v`

Expected: 测试应该 PASS（文件不存在时 `Capture::from_file` 返回 `Err`，当前代码会传播这个错误）。这说明文件打开错误没问题，问题只在读取循环内。

- [ ] **Step 4: Fix the error-swallowing loop**

将 `server/ts-capture/src/pcap_file.rs` 中的 `while let Ok(packet) = cap.next_packet()` 替换为显式 match：

```rust
            let mut count: u64 = 0;
            loop {
                match cap.next_packet() {
                    Ok(packet) => {
                        let ts = packet.header.ts;
                        let timestamp_us =
                            ts.tv_sec as i64 * 1_000_000 + ts.tv_usec as i64;

                        let raw = RawPacket {
                            timestamp_us,
                            caplen: packet.header.caplen,
                            wirelen: packet.header.len,
                            link_type,
                            data: Bytes::copy_from_slice(packet.data),
                        };

                        if tx.blocking_send(raw).is_err() {
                            tracing::debug!("pcap-file: channel closed, stopping");
                            break;
                        }

                        count += 1;
                        metrics.counter(Metric::CapturePacketsReceived).add(1);
                    }
                    Err(pcap::Error::NoMorePackets) => {
                        tracing::debug!("pcap-file: end of file reached");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("pcap-file: read error: {e}");
                        return Err(e.into());
                    }
                }
            }
```

- [ ] **Step 5: Write truncated pcap file test**

创建一个截断的 pcap 文件用于测试。在 `server/ts-capture/src/pcap_file.rs` 的测试模块中添加：

```rust
    #[tokio::test]
    async fn test_truncated_pcap_file_returns_error() {
        // Create a minimal pcap file header (24 bytes) but truncate the packet data.
        // pcap global header: magic(4) + version_major(2) + version_minor(2)
        //   + thiszone(4) + sigfigs(4) + snaplen(4) + link_type(4) = 24 bytes
        let mut header = Vec::new();
        header.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes()); // magic (little-endian)
        header.extend_from_slice(&2u16.to_le_bytes()); // version major
        header.extend_from_slice(&4u16.to_le_bytes()); // version minor
        header.extend_from_slice(&0i32.to_le_bytes()); // thiszone
        header.extend_from_slice(&0u32.to_le_bytes()); // sigfigs
        header.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
        header.extend_from_slice(&1u32.to_le_bytes()); // link type (Ethernet)
        // Add a packet header claiming 100 bytes but only write 10
        header.extend_from_slice(&0u32.to_le_bytes()); // ts_sec
        header.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
        header.extend_from_slice(&100u32.to_le_bytes()); // caplen (claiming 100)
        header.extend_from_slice(&100u32.to_le_bytes()); // orig_len
        header.extend_from_slice(&[0u8; 10]); // only 10 bytes of data (truncated)

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.pcap");
        std::fs::write(&path, &header).unwrap();

        let source = PcapFileSource::new(path);
        let (tx, _rx) = mpsc::channel(16);
        let metrics = MetricsWorker::noop();
        let result = Box::new(source).run(tx, metrics).await;
        // After the fix, truncated file should return an error, not Ok(())
        assert!(result.is_err(), "truncated pcap file should return error");
    }
```

需要在 `Cargo.toml` 中添加 `tempfile` 为 dev-dependency：

```toml
[dev-dependencies]
tempfile = "3"
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 6: Run tests**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-capture -v`

Expected: 两个测试都 PASS。

- [ ] **Step 7: Commit**

```bash
git add server/ts-capture/src/pcap_file.rs server/ts-capture/Cargo.toml server/ts-common/src/internal_metrics.rs
git commit -m "fix(ts-capture): distinguish EOF from read errors in PcapFileSource

PcapFileSource previously used while-let-Ok which treated all pcap errors
as normal EOF. Now explicitly matches NoMorePackets vs real errors."
```

---

### Task 2: 强化链路层解析（中优先级）

**Files:**
- Modify: `server/ts-capture/src/packet.rs`
- Test: `server/ts-capture/src/packet.rs` (inline tests)

当前 `ip_offset()` 只处理 3 种 link type，且 Ethernet 固定 14 字节不支持 VLAN。需要支持：
- VLAN (802.1Q) — EtherType 0x8100，额外 4 字节
- QinQ (802.1ad) — EtherType 0x88a8，额外 8 字节
- Linux cooked capture v1 (LINKTYPE 113) — 16 字节头
- Linux cooked capture v2 (LINKTYPE 276) — 20 字节头

将 `ip_offset()` 改为 `strip_link_layer()` 返回 IP 数据的起始偏移量，能正确解析 Ethernet EtherType。

- [ ] **Step 1: Write failing tests for all link layer types**

在 `server/ts-capture/src/packet.rs` 末尾添加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_packet(link_type: u32, data: &[u8]) -> RawPacket {
        RawPacket {
            timestamp_us: 0,
            caplen: data.len() as u32,
            wirelen: data.len() as u32,
            link_type,
            data: Bytes::copy_from_slice(data),
        }
    }

    #[test]
    fn test_ip_offset_raw() {
        let pkt = make_packet(LINKTYPE_RAW, &[0x45; 20]); // IPv4 header
        assert_eq!(pkt.ip_offset(), Some(0));
    }

    #[test]
    fn test_ip_offset_null() {
        let pkt = make_packet(LINKTYPE_NULL, &[2, 0, 0, 0, 0x45]); // AF_INET + IPv4
        assert_eq!(pkt.ip_offset(), Some(4));
    }

    #[test]
    fn test_ip_offset_ethernet_plain() {
        // dst(6) + src(6) + EtherType 0x0800 (IPv4) + IP header start
        let mut data = vec![0u8; 12]; // dst + src MACs
        data.extend_from_slice(&[0x08, 0x00]); // EtherType: IPv4
        data.push(0x45); // IP version+IHL
        let pkt = make_packet(LINKTYPE_ETHERNET, &data);
        assert_eq!(pkt.ip_offset(), Some(14));
    }

    #[test]
    fn test_ip_offset_ethernet_vlan() {
        let mut data = vec![0u8; 12]; // dst + src MACs
        data.extend_from_slice(&[0x81, 0x00]); // EtherType: 802.1Q VLAN
        data.extend_from_slice(&[0x00, 0x01]); // VLAN TCI
        data.extend_from_slice(&[0x08, 0x00]); // inner EtherType: IPv4
        data.push(0x45);
        let pkt = make_packet(LINKTYPE_ETHERNET, &data);
        assert_eq!(pkt.ip_offset(), Some(18));
    }

    #[test]
    fn test_ip_offset_ethernet_qinq() {
        let mut data = vec![0u8; 12]; // dst + src MACs
        data.extend_from_slice(&[0x88, 0xa8]); // EtherType: 802.1ad QinQ
        data.extend_from_slice(&[0x00, 0x01]); // outer VLAN TCI
        data.extend_from_slice(&[0x81, 0x00]); // inner 802.1Q
        data.extend_from_slice(&[0x00, 0x02]); // inner VLAN TCI
        data.extend_from_slice(&[0x08, 0x00]); // EtherType: IPv4
        data.push(0x45);
        let pkt = make_packet(LINKTYPE_ETHERNET, &data);
        assert_eq!(pkt.ip_offset(), Some(22));
    }

    #[test]
    fn test_ip_offset_linux_cooked_v1() {
        // SLL header: 16 bytes (type(2) + arphrd(2) + addr_len(2) + addr(8) + proto(2))
        let mut data = vec![0u8; 14]; // first 14 bytes of SLL header
        data.extend_from_slice(&[0x08, 0x00]); // protocol: IPv4
        data.push(0x45);
        let pkt = make_packet(LINKTYPE_LINUX_SLL, &data);
        assert_eq!(pkt.ip_offset(), Some(16));
    }

    #[test]
    fn test_ip_offset_linux_cooked_v2() {
        // SLL2 header: 20 bytes
        let mut data = vec![0u8; 20];
        data.push(0x45);
        let pkt = make_packet(LINKTYPE_LINUX_SLL2, &data);
        assert_eq!(pkt.ip_offset(), Some(20));
    }

    #[test]
    fn test_ip_offset_unsupported() {
        let pkt = make_packet(999, &[0u8; 20]);
        assert_eq!(pkt.ip_offset(), None);
    }

    #[test]
    fn test_ip_offset_ethernet_non_ip_returns_none() {
        let mut data = vec![0u8; 12]; // dst + src MACs
        data.extend_from_slice(&[0x08, 0x06]); // EtherType: ARP (not IP)
        data.extend_from_slice(&[0u8; 28]); // ARP data
        let pkt = make_packet(LINKTYPE_ETHERNET, &data);
        assert_eq!(pkt.ip_offset(), None); // ARP packets should be filtered
    }

    #[test]
    fn test_ip_offset_ethernet_too_short() {
        let data = vec![0u8; 10]; // too short for Ethernet header
        let pkt = make_packet(LINKTYPE_ETHERNET, &data);
        assert_eq!(pkt.ip_offset(), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-capture -- tests -v`

Expected: VLAN、QinQ、Linux cooked、non-IP、too-short 测试会 FAIL，因为当前代码不支持这些。

- [ ] **Step 3: Implement enhanced link-layer parsing**

将 `server/ts-capture/src/packet.rs` 替换为：

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
    /// Raw packet data starting at the link layer (or IP for LINKTYPE_RAW).
    pub data: Bytes,
}

// Well-known pcap link types.
pub const LINKTYPE_NULL: u32 = 0; // BSD loopback (4-byte AF header)
pub const LINKTYPE_ETHERNET: u32 = 1;
pub const LINKTYPE_RAW: u32 = 101;
pub const LINKTYPE_LINUX_SLL: u32 = 113; // Linux cooked capture v1
pub const LINKTYPE_LINUX_SLL2: u32 = 276; // Linux cooked capture v2

// EtherType constants.
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_IPV6: u16 = 0x86DD;
const ETHERTYPE_VLAN: u16 = 0x8100; // 802.1Q
const ETHERTYPE_QINQ: u16 = 0x88A8; // 802.1ad

impl RawPacket {
    /// Return the byte offset where the IP header starts, based on link type.
    /// Returns `None` for unsupported link types or non-IP packets.
    pub fn ip_offset(&self) -> Option<usize> {
        match self.link_type {
            LINKTYPE_NULL => Some(4),
            LINKTYPE_RAW => Some(0),
            LINKTYPE_ETHERNET => self.ethernet_ip_offset(),
            LINKTYPE_LINUX_SLL => {
                // SLL header is 16 bytes; protocol field at bytes 14-15
                if self.data.len() < 16 {
                    return None;
                }
                let proto = u16::from_be_bytes([self.data[14], self.data[15]]);
                if proto == ETHERTYPE_IPV4 || proto == ETHERTYPE_IPV6 {
                    Some(16)
                } else {
                    None
                }
            }
            LINKTYPE_LINUX_SLL2 => {
                // SLL2 header is 20 bytes; protocol field at bytes 0-1
                if self.data.len() < 20 {
                    return None;
                }
                let proto = u16::from_be_bytes([self.data[0], self.data[1]]);
                if proto == ETHERTYPE_IPV4 || proto == ETHERTYPE_IPV6 {
                    Some(20)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Parse Ethernet header, handling VLAN (802.1Q) and QinQ (802.1ad) tags.
    fn ethernet_ip_offset(&self) -> Option<usize> {
        if self.data.len() < 14 {
            return None;
        }
        // EtherType is at offset 12-13 (after dst MAC 6 + src MAC 6)
        let mut offset: usize = 12;
        let mut ethertype = u16::from_be_bytes([self.data[offset], self.data[offset + 1]]);
        offset += 2; // now at 14

        // Strip VLAN tags (802.1Q and 802.1ad/QinQ)
        // Each VLAN tag: 2-byte TCI + 2-byte next EtherType
        loop {
            match ethertype {
                ETHERTYPE_VLAN | ETHERTYPE_QINQ => {
                    if self.data.len() < offset + 4 {
                        return None; // truncated VLAN tag
                    }
                    // Skip 2-byte TCI, read next EtherType
                    ethertype =
                        u16::from_be_bytes([self.data[offset + 2], self.data[offset + 3]]);
                    offset += 4;
                }
                _ => break,
            }
        }

        // Only pass through IP packets
        if ethertype == ETHERTYPE_IPV4 || ethertype == ETHERTYPE_IPV6 {
            Some(offset)
        } else {
            None
        }
    }
}
```

- [ ] **Step 4: Update lib.rs exports**

在 `server/ts-capture/src/lib.rs` 中更新 pub use 行：

```rust
pub use packet::{
    RawPacket, LINKTYPE_ETHERNET, LINKTYPE_LINUX_SLL, LINKTYPE_LINUX_SLL2, LINKTYPE_NULL,
    LINKTYPE_RAW,
};
```

- [ ] **Step 5: Run tests**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-capture -- tests -v`

Expected: 全部 PASS。

- [ ] **Step 6: Run downstream tests to verify no regression**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-protocol -v`

Expected: PASS，因为 `ts-protocol::parse_packet()` 依赖 `ip_offset()` 接口未变。

- [ ] **Step 7: Commit**

```bash
git add server/ts-capture/src/packet.rs server/ts-capture/src/lib.rs
git commit -m "feat(ts-capture): support VLAN, QinQ, and Linux cooked capture link types

ip_offset() now properly parses Ethernet EtherType to handle 802.1Q VLAN
tags, 802.1ad QinQ double-tags, and Linux cooked capture (SLL/SLL2).
Non-IP packets are filtered out instead of silently producing wrong offsets."
```

---

### Task 3: 统一 source 构造逻辑（中优先级）

**Files:**
- Create: `server/ts-capture/src/factory.rs`
- Modify: `server/ts-capture/src/lib.rs`
- Modify: `server/app/tokenscope/src/main.rs:122-139`
- Ref: `server/ts-common/src/config.rs:39-57`

当前 `main.rs` 手写 CLI 参数分支构造 source，完全绕过了 `CaptureSourceConfig`。需要提供一个 `CaptureSourceConfig -> Box<dyn CaptureSource>` 的工厂函数，让配置和 trait 闭环。CLI 参数变为覆盖配置的语法糖。

- [ ] **Step 1: Write the factory function test**

在 `server/ts-capture/src/factory.rs` 中：

```rust
use ts_common::config::CaptureSourceConfig;

use crate::source::CaptureSource;
use crate::pcap_file::PcapFileSource;
use crate::pcap_live::PcapLiveSource;

/// Build a CaptureSource from config. Returns an error for unsupported source types.
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
        CaptureSourceConfig::CloudProbe { endpoint } => Err(crate::CaptureError::Other(
            format!("cloud-probe source not yet implemented (endpoint: {endpoint})")
        )),
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
        let source = build_source(&config);
        assert!(source.is_ok());
    }

    #[test]
    fn test_build_pcap_live_source() {
        let config = CaptureSourceConfig::Pcap {
            interface: "lo0".to_string(),
            bpf_filter: None,
            snaplen: 65535,
        };
        let source = build_source(&config);
        assert!(source.is_ok());
    }

    #[test]
    fn test_build_cloud_probe_source_returns_error() {
        let config = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
        };
        let source = build_source(&config);
        assert!(source.is_err());
    }
}
```

- [ ] **Step 2: Register the module and export**

在 `server/ts-capture/src/lib.rs` 中添加：

```rust
mod factory;
pub use factory::build_source;
```

- [ ] **Step 3: Run tests**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-capture -- factory -v`

Expected: 3 tests PASS。

- [ ] **Step 4: Refactor main.rs to use build_source**

将 `server/app/tokenscope/src/main.rs` 中 source 构造部分（约 L122-139）替换为：

```rust
    // Determine capture source: CLI overrides > config sources
    let source_configs: Vec<CaptureSourceConfig> = if let Some(pcap_file) = &args.pcap_file {
        vec![CaptureSourceConfig::PcapFile {
            path: pcap_file.to_string_lossy().to_string(),
            realtime: false,
        }]
    } else if let Some(interface) = &args.interface {
        vec![CaptureSourceConfig::Pcap {
            interface: interface.clone(),
            bpf_filter: args.bpf_filter.clone(),
            snaplen: args.snaplen,
        }]
    } else if !config.capture.sources.is_empty() {
        config.capture.sources.clone()
    } else {
        vec![]
    };

    let source: Option<Box<dyn CaptureSource>> = if let Some(src_config) = source_configs.first() {
        match ts_capture::build_source(src_config) {
            Ok(source) => {
                tracing::info!("capture source: {src_config:?}");
                Some(source)
            }
            Err(e) => {
                tracing::error!("failed to create capture source: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };
```

需要在 main.rs 顶部加 `use ts_common::config::CaptureSourceConfig;`。

注意：当前只取 `source_configs.first()`（单 source），多 source 并行是后续工作。

- [ ] **Step 5: Run full build and existing tests**

Run: `cargo build --manifest-path server/Cargo.toml && cargo test --manifest-path server/Cargo.toml`

Expected: 编译通过，所有测试 PASS。

- [ ] **Step 6: Commit**

```bash
git add server/ts-capture/src/factory.rs server/ts-capture/src/lib.rs server/app/tokenscope/src/main.rs
git commit -m "refactor(ts-capture): unify source construction via build_source factory

CaptureSourceConfig -> Box<dyn CaptureSource> factory function replaces
hand-written CLI branches in main.rs. CLI args now override config sources
through the same construction path."
```

---

### Task 4: 增加显式 shutdown 机制（中优先级）

**Files:**
- Modify: `server/ts-capture/src/source.rs`
- Modify: `server/ts-capture/src/pcap_live.rs`
- Modify: `server/ts-capture/src/pcap_file.rs`
- Modify: `server/app/tokenscope/src/main.rs`
- Test: inline tests in source files

当前停机依赖 `abort + timeout + channel close` 的组合。需要引入 `CancellationToken`（来自 tokio-util）作为统一的 shutdown 信号，让所有 source 都有明确的取消入口。

- [ ] **Step 1: Add tokio-util dependency**

在 `server/Cargo.toml` workspace dependencies 中添加：

```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

在 `server/ts-capture/Cargo.toml` 中添加：

```toml
tokio-util.workspace = true
```

- [ ] **Step 2: Update CaptureSource trait to accept CancellationToken**

修改 `server/ts-capture/src/source.rs`：

```rust
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use ts_common::internal_metrics::MetricsWorker;

use crate::RawPacket;

/// Unified interface for all capture sources (pcap, pcap-file, cloud-probe).
///
/// Each source runs as a long-lived task, pushing [`RawPacket`]s into a channel.
/// The source is consumed when `run()` is called.
#[async_trait]
pub trait CaptureSource: Send {
    /// Run the capture source, sending packets to `tx`.
    ///
    /// Returns `Ok(())` when:
    /// - The source is exhausted (e.g., end of pcap file)
    /// - The `cancel` token is triggered
    /// - The channel `tx` is closed
    ///
    /// Returns `Err` on unrecoverable errors.
    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<RawPacket>,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()>;
}
```

- [ ] **Step 3: Update PcapLiveSource to use CancellationToken**

修改 `server/ts-capture/src/pcap_live.rs` 的 `run` 方法签名，在 timeout 分支中额外检查 `cancel.is_cancelled()`：

```rust
    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<RawPacket>,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        let interface = self.interface.clone();
        let bpf_filter = self.bpf_filter.clone();
        let snaplen = self.snaplen;

        let result = tokio::task::spawn_blocking(move || -> crate::Result<()> {
            // ... (device lookup and capture setup unchanged) ...

            let mut count: u64 = 0;
            loop {
                if cancel.is_cancelled() {
                    tracing::debug!("pcap-live: cancellation requested, stopping");
                    break;
                }

                match cap.next_packet() {
                    Ok(packet) => {
                        // ... (packet construction unchanged) ...

                        if tx.blocking_send(raw).is_err() {
                            tracing::debug!("pcap-live: channel closed, stopping");
                            break;
                        }

                        count += 1;
                        metrics.counter(Metric::CapturePacketsReceived).inc();
                    }
                    Err(pcap::Error::TimeoutExpired) => {
                        if cancel.is_cancelled() || tx.is_closed() {
                            tracing::debug!("pcap-live: shutdown during timeout, stopping");
                            break;
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::error!("pcap-live: capture error: {e}");
                        return Err(e.into());
                    }
                }
            }

            // ... (stats reporting unchanged) ...

            Ok(())
        })
        .await;

        // ... (join error handling unchanged) ...
    }
```

- [ ] **Step 4: Update PcapFileSource to use CancellationToken**

修改 `server/ts-capture/src/pcap_file.rs` 的 `run` 方法签名，在读取循环中检查 cancellation：

```rust
    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<RawPacket>,
        metrics: MetricsWorker,
        cancel: CancellationToken,
    ) -> crate::Result<()> {
        let path = self.path.clone();

        let result = tokio::task::spawn_blocking(move || -> crate::Result<()> {
            let mut cap = Capture::from_file(&path)?;
            let link_type = cap.get_datalink().0 as u32;

            tracing::info!(
                "pcap-file: opened {} (link_type={})",
                path.display(),
                link_type
            );

            let mut count: u64 = 0;
            loop {
                if cancel.is_cancelled() {
                    tracing::debug!("pcap-file: cancellation requested, stopping");
                    break;
                }

                match cap.next_packet() {
                    Ok(packet) => {
                        let ts = packet.header.ts;
                        let timestamp_us =
                            ts.tv_sec as i64 * 1_000_000 + ts.tv_usec as i64;

                        let raw = RawPacket {
                            timestamp_us,
                            caplen: packet.header.caplen,
                            wirelen: packet.header.len,
                            link_type,
                            data: Bytes::copy_from_slice(packet.data),
                        };

                        if tx.blocking_send(raw).is_err() {
                            tracing::debug!("pcap-file: channel closed, stopping");
                            break;
                        }

                        count += 1;
                        metrics.counter(Metric::CapturePacketsReceived).add(1);
                    }
                    Err(pcap::Error::NoMorePackets) => {
                        tracing::debug!("pcap-file: end of file reached");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("pcap-file: read error: {e}");
                        return Err(e.into());
                    }
                }
            }

            tracing::info!("pcap-file: finished reading {} packets", count);
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(join_err) => Err(crate::CaptureError::Other(format!(
                "pcap-file task panicked: {join_err}"
            ))),
        }
    }
```

- [ ] **Step 5: Update main.rs to use CancellationToken**

修改 `server/app/tokenscope/src/main.rs`，在 capture 启动处引入 token，Ctrl+C 时取消：

```rust
    // 添加到 imports
    use tokio_util::sync::CancellationToken;

    // 在 source 启动前创建 token
    let cancel = CancellationToken::new();

    // 启动 capture source
    let capture_cancel = cancel.clone();
    let capture_handle = tokio::spawn(async move {
        if let Err(e) = source.run(raw_tx, capture_metrics, capture_cancel).await {
            tracing::error!("capture source error: {e}");
        }
    });

    // 在 Ctrl+C handler 中，替换 abort 为 cancel:
    // ...
    _ = tokio::signal::ctrl_c() => {
        tracing::info!("received Ctrl+C, stopping...");
        cancel.cancel(); // signal all sources to stop
        break;
    }

    // 替换 capture_handle.abort() 为等待优雅退出（带超时）：
    tokio::select! {
        _ = capture_handle => {
            tracing::debug!("capture source stopped gracefully");
        }
        _ = tokio::time::sleep(Duration::from_secs(3)) => {
            tracing::warn!("capture source did not stop in time, aborting");
        }
    }
```

- [ ] **Step 6: Update factory.rs and existing tests**

更新所有测试中的 `run()` 调用，添加 `CancellationToken::new()` 参数。更新 `factory.rs` 无需改动（它只构造 source，不调用 run）。

更新 Task 1 中的 pcap_file 测试：

```rust
    #[tokio::test]
    async fn test_pcap_file_not_found_returns_error() {
        let source = PcapFileSource::new(PathBuf::from("/nonexistent/test.pcap"));
        let (tx, _rx) = mpsc::channel(16);
        let metrics = MetricsWorker::noop();
        let cancel = CancellationToken::new();
        let result = Box::new(source).run(tx, metrics, cancel).await;
        assert!(result.is_err(), "should return error for missing file");
    }
```

- [ ] **Step 7: Run all tests**

Run: `cargo build --manifest-path server/Cargo.toml && cargo test --manifest-path server/Cargo.toml`

Expected: 编译通过，所有测试 PASS。

- [ ] **Step 8: Commit**

```bash
git add server/Cargo.toml server/ts-capture/Cargo.toml server/ts-capture/src/source.rs \
  server/ts-capture/src/pcap_live.rs server/ts-capture/src/pcap_file.rs \
  server/app/tokenscope/src/main.rs
git commit -m "refactor(ts-capture): add explicit CancellationToken for graceful shutdown

Replace abort-based shutdown with tokio-util CancellationToken. All capture
sources now check the token in their read loops. main.rs signals cancellation
on Ctrl+C and waits up to 3s for graceful exit before giving up."
```

---

### Task 5: 补齐 source 退出行为测试（低优先级）

**Files:**
- Modify: `server/ts-capture/src/pcap_file.rs` (add tests)
- Modify: `server/ts-capture/src/pcap_live.rs` (add tests)

验证 channel 关闭和 cancellation token 触发时 source 能正确退出。

- [ ] **Step 1: Write channel-close test for PcapFileSource**

在 `server/ts-capture/src/pcap_file.rs` 的测试模块中添加：

```rust
    #[tokio::test]
    async fn test_pcap_file_channel_close_stops_reading() {
        // Create a valid pcap file with enough packets that the source won't finish instantly.
        // We'll use a minimal valid pcap with one packet.
        let mut data = Vec::new();
        // pcap global header (little-endian)
        data.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes()); // magic
        data.extend_from_slice(&2u16.to_le_bytes());           // version major
        data.extend_from_slice(&4u16.to_le_bytes());           // version minor
        data.extend_from_slice(&0i32.to_le_bytes());           // thiszone
        data.extend_from_slice(&0u32.to_le_bytes());           // sigfigs
        data.extend_from_slice(&65535u32.to_le_bytes());       // snaplen
        data.extend_from_slice(&1u32.to_le_bytes());           // link type (Ethernet)
        // One valid packet: 64 bytes of zeros (minimal Ethernet frame)
        let pkt_data = [0u8; 64];
        data.extend_from_slice(&1u32.to_le_bytes()); // ts_sec
        data.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
        data.extend_from_slice(&(pkt_data.len() as u32).to_le_bytes()); // caplen
        data.extend_from_slice(&(pkt_data.len() as u32).to_le_bytes()); // orig_len
        data.extend_from_slice(&pkt_data);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one_packet.pcap");
        std::fs::write(&path, &data).unwrap();

        // Create a channel and immediately drop the receiver.
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let source = PcapFileSource::new(path);
        let metrics = MetricsWorker::noop();
        let cancel = CancellationToken::new();
        let result = Box::new(source).run(tx, metrics, cancel).await;
        // Should exit gracefully (Ok) when channel is closed.
        assert!(result.is_ok(), "should return Ok when channel is closed");
    }
```

- [ ] **Step 2: Write cancellation test for PcapFileSource**

```rust
    #[tokio::test]
    async fn test_pcap_file_cancellation_stops_reading() {
        // Use the same valid pcap file as above.
        // ... (same file creation code) ...

        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        cancel.cancel(); // Pre-cancel before starting

        let source = PcapFileSource::new(path);
        let metrics = MetricsWorker::noop();
        let result = Box::new(source).run(tx, metrics, cancel).await;
        assert!(result.is_ok(), "should return Ok when cancelled");
    }
```

- [ ] **Step 3: Run all ts-capture tests**

Run: `cargo test --manifest-path server/Cargo.toml -p ts-capture -v`

Expected: 全部 PASS。

- [ ] **Step 4: Run full workspace tests**

Run: `cargo test --manifest-path server/Cargo.toml`

Expected: 全部 PASS，无回归。

- [ ] **Step 5: Commit**

```bash
git add server/ts-capture/src/pcap_file.rs
git commit -m "test(ts-capture): add source exit behavior tests

Verify PcapFileSource exits gracefully on channel close and cancellation
token trigger."
```

---

## 任务依赖关系

```
Task 1 (错误吞没修复) ─── 独立，最高优先级
Task 2 (链路层解析)   ─── 独立，可与 Task 1 并行
Task 3 (source 工厂)  ─── 依赖 Task 1, 2 完成后执行（避免冲突）
Task 4 (shutdown)     ─── 依赖 Task 1（修改同一文件），建议在 Task 3 后
Task 5 (退出行为测试) ─── 依赖 Task 4（需要 CancellationToken 参数）
```

推荐执行顺序：Task 1 → Task 2 → Task 3 → Task 4 → Task 5

## 不在本次范围内

- 多 source 并行运行（`source_configs` 目前只取 first）
- cloud-probe ZMQ 实现
- `PcapFile { realtime }` 回放模式
- 包复制性能优化（`Bytes::copy_from_slice` → zero-copy）
- `CaptureSource` trait 解耦 tokio mpsc（当前作为内部运行接口仍然够用）
