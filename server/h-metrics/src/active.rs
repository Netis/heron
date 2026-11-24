//! In-flight call table behind the `active_calls_*` metrics.
//!
//! A call is *open* from its Start until the Complete with the same
//! [`CallKey`]. Two earlier designs got this wrong in production, and the
//! shape of this table is the answer to both:
//!
//! * **Counting +1/−1 per dimension** paired a Start with its Complete by
//!   `(wire_api, model, server_ip)`. The Start carries the model the request
//!   named and the Complete the one the response named; a server that answers
//!   `DeepSeek-V4.1-Flash` with `dsv4.1-flash` made every call +1 one counter
//!   and −1 (clamped at zero) another. Here the Complete removes the entry its
//!   Start made, and decrements the dimensions *stored with it*.
//! * **One table per metrics shard.** Shards are picked by a hash that
//!   includes the model, so the same mismatch sent the Start and the Complete
//!   to different shards; and even without it, each shard's "global" count saw
//!   only its own share of models. The table is shared by every shard, so a
//!   sample taken anywhere is the whole picture.
//!
//! A Start whose Complete never arrives (connection dropped, exchange evicted
//! by the joiner) would still stay open forever. [`ActiveCalls::expire`]
//! closes anything older than [`MAX_OPEN_US`] of event time, so the worst a
//! lost Complete can do is linger for that long.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use h_llm::model::CallKey;

use crate::aggregator::DimensionKey;

/// How long a call may stay open without its Complete, in event time.
///
/// Deliberately far above any real call. The joiner only gives up on an
/// exchange after ten minutes of *silence*, so a stream that keeps producing
/// is never evicted there and can legitimately run long; one that outlives
/// this bound is dropped from the count early, which is the cheaper error.
pub const MAX_OPEN_US: i64 = 3_600_000_000;

/// The table, shared by every metrics shard of a pipeline.
#[derive(Clone, Default)]
pub struct ActiveCalls(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    open: HashMap<CallKey, Open>,
    counts: HashMap<DimensionKey, u32>,
}

struct Open {
    dims: [DimensionKey; 4],
    start_us: i64,
}

impl ActiveCalls {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a call. Returns the open-call count of each of `dims` afterwards.
    /// A repeated Start for a key that is already open is not counted twice.
    pub(crate) fn start(&self, key: CallKey, dims: [DimensionKey; 4], start_us: i64) -> [u32; 4] {
        let mut g = self.0.lock().unwrap();
        let inner = &mut *g;
        if !inner.open.contains_key(&key) {
            for d in &dims {
                *inner.counts.entry(d.clone()).or_insert(0) += 1;
            }
            inner.open.insert(
                key,
                Open {
                    dims: dims.clone(),
                    start_us,
                },
            );
        }
        dims.map(|d| inner.counts.get(&d).copied().unwrap_or(0))
    }

    /// Close a call. A Complete with no open Start — capture began mid-call,
    /// or the Start was already expired — changes nothing.
    pub(crate) fn complete(&self, key: &CallKey) {
        let mut g = self.0.lock().unwrap();
        if let Some(open) = g.open.remove(key) {
            decrement(&mut g.counts, &open.dims);
        }
    }

    /// Close every call on `source_id` that opened more than [`MAX_OPEN_US`]
    /// before `now_us`. Returns how many were closed.
    pub(crate) fn expire(&self, source_id: &str, now_us: i64) -> usize {
        let mut g = self.0.lock().unwrap();
        let inner = &mut *g;
        let stale: Vec<CallKey> = inner
            .open
            .iter()
            .filter(|(k, o)| k.source_id == source_id && now_us - o.start_us > MAX_OPEN_US)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            if let Some(open) = inner.open.remove(k) {
                decrement(&mut inner.counts, &open.dims);
            }
        }
        stale.len()
    }

    /// Calls currently open, across every shard.
    pub fn open_count(&self) -> usize {
        self.0.lock().unwrap().open.len()
    }
}

fn decrement(counts: &mut HashMap<DimensionKey, u32>, dims: &[DimensionKey; 4]) {
    for d in dims {
        if let Some(n) = counts.get_mut(d) {
            *n -= 1;
            if *n == 0 {
                counts.remove(d);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn key(port: u16, t: i64) -> CallKey {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        CallKey {
            source_id: "src".into(),
            client: (ip, port),
            server: ("10.0.0.2".parse().unwrap(), 443),
            request_time_us: t,
        }
    }

    fn dims(model: &str) -> [DimensionKey; 4] {
        crate::aggregator::dimension_keys("src", "openai-chat", model, "10.0.0.2")
    }

    #[test]
    fn complete_closes_the_dimensions_its_start_opened() {
        let a = ActiveCalls::new();
        a.start(key(1, 0), dims("requested-name"), 0);
        // The Complete knows nothing about the requested name; it does not
        // need to.
        a.complete(&key(1, 0));
        assert_eq!(a.open_count(), 0);
        assert_eq!(a.start(key(2, 1), dims("requested-name"), 1), [1, 1, 1, 1]);
    }

    #[test]
    fn global_count_spans_every_model() {
        let a = ActiveCalls::new();
        a.start(key(1, 0), dims("m1"), 0);
        let after = a.start(key(2, 0), dims("m2"), 0);
        // exact tier and model tier see one call each; server and global, two
        assert_eq!(after, [1, 1, 2, 2]);
    }

    #[test]
    fn clones_share_one_table() {
        let shard_a = ActiveCalls::new();
        let shard_b = shard_a.clone();
        shard_a.start(key(1, 0), dims("m1"), 0);
        assert_eq!(shard_b.start(key(2, 0), dims("m2"), 0)[3], 2);
        shard_b.complete(&key(1, 0));
        assert_eq!(shard_a.open_count(), 1);
    }

    #[test]
    fn complete_without_start_and_repeated_start_change_nothing() {
        let a = ActiveCalls::new();
        a.complete(&key(9, 0));
        a.start(key(1, 0), dims("m"), 0);
        assert_eq!(a.start(key(1, 0), dims("m"), 0), [1, 1, 1, 1]);
        assert_eq!(a.open_count(), 1);
    }

    #[test]
    fn a_start_that_never_completes_expires() {
        let a = ActiveCalls::new();
        a.start(key(1, 0), dims("m"), 0);
        a.start(key(2, MAX_OPEN_US), dims("m"), MAX_OPEN_US);
        assert_eq!(
            a.expire("src", MAX_OPEN_US),
            0,
            "exactly at the bound stays"
        );
        assert_eq!(a.expire("other-source", MAX_OPEN_US + 1), 0);
        assert_eq!(a.expire("src", MAX_OPEN_US + 1), 1);
        assert_eq!(a.open_count(), 1);
        // the late Complete of the expired call is a no-op, not a double close
        a.complete(&key(1, 0));
        assert_eq!(a.start(key(3, 0), dims("m"), MAX_OPEN_US + 2)[3], 2);
    }
}
