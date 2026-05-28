//! Runtime registry of capture data sources.
//!
//! Two concerns live side by side in this project:
//!
//! * [`internal_metrics`](crate::internal_metrics) — aggregated counters
//!   reported to logs, keyed by worker role.
//! * this module — a *catalog* of live capture sources, keyed by a stable
//!   string identity (interface name, file path, or cloud-probe UUID) so the
//!   HTTP API can surface "who is sending us data right now".
//!
//! The catalog is populated two ways:
//!
//! * Static sources (pcap live / pcap-file / cloud-probe receiver) are
//!   registered once at startup via [`SourceRegistry::register_static`].
//! * Cloud-probe peers appear at runtime the first time a batch from a new
//!   UUID is parsed; [`SourceRegistry::ensure_peer`] inserts one entry
//!   linked to its receiver via [`SourceSnapshot::parent_key`].
//!
//! The hot path is [`SourceRegistry::touch`] called on every forwarded
//! packet. It takes only a read lock on the map and bumps three atomics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Classification of a data source entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Live pcap interface.
    Pcap,
    /// Offline pcap file replay.
    PcapFile,
    /// ZMQ PullSocket bound to a local endpoint; one per configured
    /// `cloud-probe` source. Acts as a parent for discovered peer entries.
    CloudProbeReceiver,
    /// One remote probe identified by the UUID in its batch header.
    /// Discovered at runtime — never pre-registered.
    CloudProbePeer,
}

/// Serializable point-in-time view of one source.
#[derive(Debug, Clone, Serialize)]
pub struct SourceSnapshot {
    pub key: String,
    pub kind: SourceKind,
    pub endpoint: String,
    pub parent_key: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub packets: u64,
    pub heartbeats: u64,
}

/// Internal entry. All mutable state is atomic so the hot path never takes
/// a write lock.
struct SourceEntry {
    kind: SourceKind,
    endpoint: String,
    parent_key: Option<String>,
    first_seen_ms: AtomicI64,
    last_seen_ms: AtomicI64,
    packets: AtomicU64,
    heartbeats: AtomicU64,
}

impl SourceEntry {
    fn new(kind: SourceKind, endpoint: String, parent_key: Option<String>) -> Self {
        Self {
            kind,
            endpoint,
            parent_key,
            first_seen_ms: AtomicI64::new(0),
            last_seen_ms: AtomicI64::new(0),
            packets: AtomicU64::new(0),
            heartbeats: AtomicU64::new(0),
        }
    }

    fn snapshot(&self, key: String) -> SourceSnapshot {
        SourceSnapshot {
            key,
            kind: self.kind,
            endpoint: self.endpoint.clone(),
            parent_key: self.parent_key.clone(),
            first_seen_ms: self.first_seen_ms.load(Ordering::Relaxed),
            last_seen_ms: self.last_seen_ms.load(Ordering::Relaxed),
            packets: self.packets.load(Ordering::Relaxed),
            heartbeats: self.heartbeats.load(Ordering::Relaxed),
        }
    }

    /// Record activity. Sets `first_seen_ms` on the 0→non-zero transition;
    /// bumps the relevant counter; updates `last_seen_ms` monotonically
    /// (a stale concurrent writer never clobbers a newer value).
    fn touch(&self, now_ms: i64, is_heartbeat: bool) {
        let _ = self.first_seen_ms.compare_exchange(
            0,
            now_ms,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        // Monotonic max for last_seen_ms.
        loop {
            let cur = self.last_seen_ms.load(Ordering::Relaxed);
            if now_ms <= cur {
                break;
            }
            if self
                .last_seen_ms
                .compare_exchange_weak(cur, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        if is_heartbeat {
            self.heartbeats.fetch_add(1, Ordering::Relaxed);
        } else {
            self.packets.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Concurrent map of data sources. Cheap to clone via `Arc`.
#[derive(Default)]
pub struct SourceRegistry {
    inner: RwLock<HashMap<String, Arc<SourceEntry>>>,
}

impl SourceRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Startup-time registration. Idempotent: a second call with the same
    /// `key` is a no-op, preserving whatever counters have already accrued.
    pub fn register_static(
        &self,
        key: &str,
        kind: SourceKind,
        endpoint: &str,
        parent_key: Option<&str>,
    ) {
        let mut map = self.inner.write().unwrap();
        map.entry(key.to_string()).or_insert_with(|| {
            Arc::new(SourceEntry::new(
                kind,
                endpoint.to_string(),
                parent_key.map(|s| s.to_string()),
            ))
        });
    }

    /// Runtime insertion of a cloud-probe peer the first time its UUID is
    /// observed. Subsequent calls for the same `uuid` are no-ops.
    pub fn ensure_peer(&self, uuid: &str, receiver_key: &str) {
        // Fast path: already present → read lock only.
        if self.inner.read().unwrap().contains_key(uuid) {
            return;
        }
        let mut map = self.inner.write().unwrap();
        map.entry(uuid.to_string()).or_insert_with(|| {
            Arc::new(SourceEntry::new(
                SourceKind::CloudProbePeer,
                receiver_key.to_string(),
                Some(receiver_key.to_string()),
            ))
        });
    }

    /// Hot path: record activity for an existing key. Silently no-ops if
    /// the key is unknown — callers are expected to have called
    /// `register_static` or `ensure_peer` first.
    pub fn touch(&self, key: &str, now_ms: i64, is_heartbeat: bool) {
        let entry = { self.inner.read().unwrap().get(key).cloned() };
        if let Some(entry) = entry {
            entry.touch(now_ms, is_heartbeat);
        }
    }

    /// Snapshot all entries sorted by key for stable API output.
    pub fn snapshot(&self) -> Vec<SourceSnapshot> {
        let map = self.inner.read().unwrap();
        let mut out: Vec<SourceSnapshot> = map
            .iter()
            .map(|(k, v)| v.snapshot(k.clone()))
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }
}

/// Convenience for call sites that don't already have a clock on hand.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_static_is_idempotent() {
        let reg = SourceRegistry::new();
        reg.register_static("eth0", SourceKind::Pcap, "eth0", None);
        reg.touch("eth0", 100, false);
        // Second call with the same key must not reset counters.
        reg.register_static("eth0", SourceKind::Pcap, "eth0", None);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].key, "eth0");
        assert_eq!(snap[0].packets, 1);
        assert_eq!(snap[0].first_seen_ms, 100);
    }

    #[test]
    fn ensure_peer_links_to_receiver() {
        let reg = SourceRegistry::new();
        reg.register_static(
            "tcp://0.0.0.0:5555",
            SourceKind::CloudProbeReceiver,
            "tcp://0.0.0.0:5555",
            None,
        );
        reg.ensure_peer("uuid-A", "tcp://0.0.0.0:5555");
        reg.ensure_peer("uuid-A", "tcp://0.0.0.0:5555"); // second call no-ops
        reg.ensure_peer("uuid-B", "tcp://0.0.0.0:5555");

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 3);
        // Sorted by key: tcp://…, uuid-A, uuid-B.
        assert_eq!(snap[0].kind, SourceKind::CloudProbeReceiver);
        assert_eq!(snap[0].parent_key, None);
        assert_eq!(snap[1].key, "uuid-A");
        assert_eq!(snap[1].kind, SourceKind::CloudProbePeer);
        assert_eq!(snap[1].parent_key.as_deref(), Some("tcp://0.0.0.0:5555"));
        assert_eq!(snap[2].key, "uuid-B");
    }

    #[test]
    fn touch_sets_first_seen_only_once() {
        let reg = SourceRegistry::new();
        reg.register_static("eth0", SourceKind::Pcap, "eth0", None);
        reg.touch("eth0", 100, false);
        reg.touch("eth0", 200, false);
        let snap = reg.snapshot();
        assert_eq!(snap[0].first_seen_ms, 100);
        assert_eq!(snap[0].last_seen_ms, 200);
    }

    #[test]
    fn touch_last_seen_is_monotonic() {
        let reg = SourceRegistry::new();
        reg.register_static("eth0", SourceKind::Pcap, "eth0", None);
        reg.touch("eth0", 300, false);
        // A stale timestamp must not clobber a newer one.
        reg.touch("eth0", 100, false);
        let snap = reg.snapshot();
        assert_eq!(snap[0].last_seen_ms, 300);
    }

    #[test]
    fn touch_distinguishes_heartbeat_and_packet() {
        let reg = SourceRegistry::new();
        reg.register_static("eth0", SourceKind::Pcap, "eth0", None);
        reg.touch("eth0", 100, false);
        reg.touch("eth0", 101, false);
        reg.touch("eth0", 102, true);
        let snap = reg.snapshot();
        assert_eq!(snap[0].packets, 2);
        assert_eq!(snap[0].heartbeats, 1);
    }

    #[test]
    fn touch_unknown_key_is_noop() {
        let reg = SourceRegistry::new();
        reg.touch("nonexistent", 100, false);
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn snapshot_is_sorted_by_key() {
        let reg = SourceRegistry::new();
        reg.register_static("zeta", SourceKind::Pcap, "zeta", None);
        reg.register_static("alpha", SourceKind::Pcap, "alpha", None);
        reg.register_static("mike", SourceKind::Pcap, "mike", None);
        let snap = reg.snapshot();
        let keys: Vec<&str> = snap.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "mike", "zeta"]);
    }

    #[test]
    fn concurrent_touch_accumulates_correctly() {
        let reg = SourceRegistry::new();
        reg.register_static("eth0", SourceKind::Pcap, "eth0", None);

        let handles: Vec<_> = (0..10)
            .map(|thread_idx| {
                let reg = reg.clone();
                std::thread::spawn(move || {
                    for i in 0..1000 {
                        let ts = (thread_idx * 1000 + i) as i64;
                        reg.touch("eth0", ts, false);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let snap = reg.snapshot();
        assert_eq!(snap[0].packets, 10_000);
        // Largest ts used: 9 * 1000 + 999 = 9999.
        assert_eq!(snap[0].last_seen_ms, 9999);
    }

    #[test]
    fn concurrent_ensure_peer_creates_single_entry() {
        let reg = SourceRegistry::new();
        reg.register_static(
            "tcp://0.0.0.0:5555",
            SourceKind::CloudProbeReceiver,
            "tcp://0.0.0.0:5555",
            None,
        );

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let reg = reg.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        reg.ensure_peer("same-uuid", "tcp://0.0.0.0:5555");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let snap = reg.snapshot();
        // One receiver + one peer.
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.iter().filter(|s| s.key == "same-uuid").count(), 1);
    }

    #[test]
    fn now_ms_is_positive() {
        assert!(now_ms() > 1_700_000_000_000);
    }
}
