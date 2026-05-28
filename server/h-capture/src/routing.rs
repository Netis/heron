//! Transparent routing sender that distributes [`RawPacket`]s across multiple
//! dispatcher channels by `hash(source_id)`. When there is only one channel
//! (the common case with `dispatcher_count = 1`), the hash is skipped entirely.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tokio::sync::mpsc;

use crate::RawPacket;

/// A sender that routes [`RawPacket`]s to one of N underlying channels,
/// chosen by `hash(source_id) % N`. Implements the same `send` / `blocking_send`
/// interface as `mpsc::Sender<RawPacket>` so capture sources are unaware of
/// the routing.
#[derive(Clone, Debug)]
pub struct RoutingSender {
    txs: Vec<mpsc::Sender<RawPacket>>,
}

impl RoutingSender {
    /// Create a new routing sender. Panics if `txs` is empty.
    pub fn new(txs: Vec<mpsc::Sender<RawPacket>>) -> Self {
        assert!(
            !txs.is_empty(),
            "RoutingSender requires at least one channel"
        );
        Self { txs }
    }

    /// Create a routing sender wrapping a single channel (no hashing).
    pub fn single(tx: mpsc::Sender<RawPacket>) -> Self {
        Self { txs: vec![tx] }
    }

    /// Send a packet, routing by `source_id` hash.
    pub async fn send(&self, pkt: RawPacket) -> Result<(), mpsc::error::SendError<RawPacket>> {
        let tx = &self.txs[self.index(&pkt.source_id)];
        tx.send(pkt).await
    }

    /// Blocking variant for use inside `spawn_blocking` tasks (pcap sources).
    pub fn blocking_send(&self, pkt: RawPacket) -> Result<(), mpsc::error::SendError<RawPacket>> {
        let tx = &self.txs[self.index(&pkt.source_id)];
        tx.blocking_send(pkt)
    }

    /// Returns true if all underlying channels are closed.
    pub fn is_closed(&self) -> bool {
        self.txs.iter().all(|tx| tx.is_closed())
    }

    #[inline]
    fn index(&self, source_id: &str) -> usize {
        if self.txs.len() == 1 {
            return 0;
        }
        let mut h = DefaultHasher::new();
        source_id.hash(&mut h);
        (h.finish() as usize) % self.txs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_channel_routes_everything() {
        let (tx, mut rx) = mpsc::channel(16);
        let router = RoutingSender::single(tx);

        let pkt = RawPacket::heartbeat(1_000_000, "source-a".into());
        router.send(pkt).await.unwrap();

        let pkt = RawPacket::heartbeat(2_000_000, "source-b".into());
        router.send(pkt).await.unwrap();

        assert_eq!(rx.recv().await.unwrap().source_id, "source-a");
        assert_eq!(rx.recv().await.unwrap().source_id, "source-b");
    }

    #[tokio::test]
    async fn same_source_id_always_routes_to_same_channel() {
        let mut txs = Vec::new();
        let mut rxs = Vec::new();
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel(16);
            txs.push(tx);
            rxs.push(rx);
        }
        let router = RoutingSender::new(txs);

        for _ in 0..10 {
            let pkt = RawPacket::heartbeat(1_000_000, "same-source".into());
            router.send(pkt).await.unwrap();
        }

        // All 10 packets should land on a single channel.
        let mut counts = vec![0usize; 4];
        for (i, rx) in rxs.iter_mut().enumerate() {
            while rx.try_recv().is_ok() {
                counts[i] += 1;
            }
        }
        let non_zero: Vec<_> = counts.iter().filter(|&&c| c > 0).collect();
        assert_eq!(non_zero.len(), 1, "all packets must go to one channel");
        assert_eq!(*non_zero[0], 10);
    }

    #[tokio::test]
    async fn different_source_ids_can_route_to_different_channels() {
        let mut txs = Vec::new();
        let mut rxs = Vec::new();
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel(64);
            txs.push(tx);
            rxs.push(rx);
        }
        let router = RoutingSender::new(txs);

        // Send packets with many distinct source_ids.
        for i in 0..100 {
            let pkt = RawPacket::heartbeat(1_000_000, format!("source-{i}"));
            router.send(pkt).await.unwrap();
        }

        let mut counts = vec![0usize; 4];
        for (i, rx) in rxs.iter_mut().enumerate() {
            while rx.try_recv().is_ok() {
                counts[i] += 1;
            }
        }
        let non_zero = counts.iter().filter(|&&c| c > 0).count();
        assert!(
            non_zero > 1,
            "100 distinct source_ids should spread across multiple channels, got counts={counts:?}"
        );
        assert_eq!(counts.iter().sum::<usize>(), 100);
    }

    #[tokio::test]
    async fn blocking_send_works() {
        let (tx, mut rx) = mpsc::channel(16);
        let router = RoutingSender::single(tx);

        // blocking_send must be called from a blocking context.
        let r = router.clone();
        tokio::task::spawn_blocking(move || {
            let pkt = RawPacket::heartbeat(1_000_000, "test".into());
            r.blocking_send(pkt).unwrap();
        })
        .await
        .unwrap();

        assert_eq!(rx.recv().await.unwrap().source_id, "test");
    }

    #[test]
    fn is_closed_reflects_all_channels() {
        let (tx1, rx1) = mpsc::channel::<RawPacket>(1);
        let (tx2, rx2) = mpsc::channel::<RawPacket>(1);
        let router = RoutingSender::new(vec![tx1, tx2]);

        assert!(!router.is_closed());
        drop(rx1);
        assert!(!router.is_closed()); // one still open
        drop(rx2);
        assert!(router.is_closed());
    }
}
