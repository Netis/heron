//! Shared read primitives: fetch whole events, decode them in Rust.
//!
//! Two rules hold for every read in this backend.
//!
//! **Decode from `_raw`, never from the extracted fields.** Search output is
//! lossy in three ways that each corrupt a struct without erroring: null
//! fields are dropped, a single-element multivalue collapses to a scalar, and
//! integers past 2^53 come back as strings. Extracted fields are for filtering
//! and aggregating; `_raw` is for reconstructing.
//!
//! **Always bound the time window.** An unbounded search consults every bucket
//! in the retention window, which is the one cost that grows without limit as
//! a deployment ages. Point lookups get their bounds from the UUIDv7 id
//! itself, with an unbounded retry as the safety net — see [`crate::spl::id_windows`]
//! for why the hint can legitimately miss.

use serde::de::DeserializeOwned;

use h_common::error::Result;
use h_storage::query::TimeRange;

use crate::client::Row;
use crate::spl::{self, raw_query, Search};
use crate::AglakeBackend;

impl AglakeBackend {
    /// Run a `| table _raw` query and decode each row into `T`.
    ///
    /// A row that fails to decode is skipped and logged rather than failing
    /// the whole query: one malformed event should cost one row in a trace
    /// view, not the entire view.
    pub(crate) async fn fetch_raw<T: DeserializeOwned>(
        &self,
        what: &'static str,
        search: &str,
        limit: usize,
        earliest: &str,
        latest: &str,
    ) -> Result<Vec<T>> {
        let result = self
            .search
            .search(&raw_query(search, limit), earliest, latest)
            .await?;
        Ok(decode_rows(what, result.rows()))
    }

    /// A point lookup bounded by the id's own UUIDv7 timestamp, retried
    /// unbounded if that window turns up nothing.
    ///
    /// The retry is not belt-and-braces: replaying a captured pcap stamps
    /// events with capture-time `_time` while their ids are minted now, so the
    /// derived window is guaranteed to miss. Costing one extra empty query on
    /// a genuine miss buys a bounded lookup on every hit.
    pub(crate) async fn fetch_raw_by_id<T: DeserializeOwned>(
        &self,
        what: &'static str,
        search: &str,
        limit: usize,
        id: &str,
    ) -> Result<Vec<T>> {
        for (earliest, latest) in spl::id_windows(id) {
            let hit: Vec<T> = self
                .fetch_raw(what, search, limit, &earliest, &latest)
                .await?;
            if !hit.is_empty() {
                return Ok(hit);
            }
        }
        self.fetch_raw(what, search, limit, "0", "0").await
    }

    /// Widen a `[start, end]` microsecond range by the configured trace skew
    /// and render it as search bounds.
    pub(crate) fn window(&self, start_us: i64, end_us: i64) -> (String, String) {
        (
            spl::epoch_secs(start_us.saturating_sub(self.trace_time_skew_us)),
            spl::epoch_secs(end_us.saturating_add(self.trace_time_skew_us)),
        )
    }

    /// One page of whole events, plus the matching total.
    ///
    /// The two come from two queries because a pipeline's reported `total` is
    /// the number of rows it *emitted*, not the number matched — after
    /// `| tail`, that would always equal the page size. They run concurrently
    /// since neither depends on the other.
    pub(crate) async fn fetch_page<T: DeserializeOwned>(
        &self,
        what: &'static str,
        search: &Search,
        sort: &Sort,
        page: u32,
        page_size: u32,
        range: &TimeRange,
    ) -> Result<(Vec<T>, u64)> {
        let page_size = page_size.max(1) as u64;
        let offset = (page.saturating_sub(1)) as u64 * page_size;
        // Offset pagination reads `offset + page_size` rows to discard all but
        // the last page_size. That is fine for the first few hundred pages and
        // ruinous past that, so the ceiling is an explicit error rather than a
        // request that quietly takes minutes.
        if offset > self.max_page_offset {
            return Err(h_common::error::AppError::Storage(format!(
                "aglake backend: page offset {offset} exceeds \
                 storage.aglake.max_page_offset ({}); narrow the time range or \
                 filters instead of paging this deep",
                self.max_page_offset
            )));
        }

        let prefix = search.build();
        let (earliest, latest) = (
            spl::epoch_secs(range.start_us),
            spl::epoch_secs(range.end_us),
        );
        let items_spl = spl::paginate(&prefix, &sort.keys, offset, page_size, &["_raw"]);
        let count_spl = spl::count_query(&prefix);

        let (items, count) = tokio::try_join!(
            self.search.search(&items_spl, &earliest, &latest),
            self.search.search(&count_spl, &earliest, &latest),
        )?;

        let total = count
            .rows()
            .first()
            .and_then(|r| r.get("n"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok((decode_rows(what, items.rows()), total))
    }
}

/// Decode `_raw` out of each row, skipping (and reporting) any that will not
/// parse. See [`AglakeBackend::fetch_raw`] for why `_raw` and not the fields.
pub(crate) fn decode_rows<T: DeserializeOwned>(what: &'static str, rows: Vec<Row>) -> Vec<T> {
    let mut out = Vec::with_capacity(rows.len());
    let mut undecodable = 0usize;
    for row in rows {
        match row.get("_raw").and_then(|v| v.as_str()) {
            Some(raw) => match serde_json::from_str::<T>(raw) {
                Ok(v) => out.push(v),
                Err(_) => undecodable += 1,
            },
            None => undecodable += 1,
        }
    }
    if undecodable > 0 {
        tracing::warn!(
            target: "aglake::read",
            query = what,
            skipped = undecodable,
            "aglake: skipped event(s) that could not be decoded"
        );
    }
    out
}

/// A validated sort specification.
///
/// `sort_by` arrives from the API as a free string, and it is spliced into the
/// query, so it is checked against a whitelist exactly as the SQL backends
/// check theirs. Every key set ends with a deterministic tie-break: rows with
/// equal sort values otherwise come back in bucket storage order, which
/// changes when hot buckets are sealed or merged — and an unstable order makes
/// offset pagination drop and repeat rows across pages.
pub(crate) struct Sort {
    pub keys: String,
}

impl Sort {
    pub(crate) fn new(
        sort_by: &str,
        sort_order: &str,
        allowed: &[(&str, &str)],
        tie_break: &[&str],
    ) -> Result<Self> {
        let Some((_, expr)) = allowed.iter().find(|(name, _)| *name == sort_by) else {
            return Err(h_common::error::AppError::Storage(format!(
                "invalid sort_by field: {sort_by}"
            )));
        };
        let sign = if sort_order.eq_ignore_ascii_case("ASC") {
            '+'
        } else {
            '-'
        };
        let mut keys = format!("{sign}{expr}");
        for t in tie_break {
            // Same direction as the primary key: any fixed choice gives a
            // total order, and matching keeps the tie-break invisible.
            keys.push_str(&format!(", {sign}{t}"));
        }
        Ok(Self { keys })
    }
}

#[cfg(test)]
mod tests {
    use crate::spl::id_windows;

    /// A UUIDv7 carries its own millisecond timestamp, which is what makes a
    /// by-id lookup prunable at all.
    #[test]
    fn id_windows_bracket_a_uuidv7_timestamp() {
        let id = uuid::Uuid::now_v7().to_string();
        let windows = id_windows(&id);
        assert!(!windows.is_empty(), "a v7 id must yield windows");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        for (earliest, latest) in &windows {
            let e: f64 = earliest.parse().unwrap();
            let l: f64 = latest.parse().unwrap();
            assert!(e < l);
            assert!(e <= now && now <= l, "window {e}..{l} excludes {now}");
        }
    }

    /// Narrow first. A point lookup costs what its window costs, so trying the
    /// widest one first would hand the common case the worst case's bill.
    #[test]
    fn id_windows_widen_in_order() {
        let id = uuid::Uuid::now_v7().to_string();
        let windows = id_windows(&id);
        assert!(
            windows.len() >= 2,
            "there must be a narrow attempt before the wide one"
        );
        let span = |w: &(String, String)| -> f64 {
            w.1.parse::<f64>().unwrap() - w.0.parse::<f64>().unwrap()
        };
        for pair in windows.windows(2) {
            assert!(
                span(&pair[0]) < span(&pair[1]),
                "windows must widen, not narrow: {:?}",
                windows
            );
        }
    }

    /// Ids Heron did not mint — a provider-supplied `turn_id`, a v4 UUID —
    /// must yield no window at all rather than a wrong one. An empty list sends
    /// the caller straight to its unbounded retry, which is correct; a wrong
    /// window would send it to a confident empty answer, which is not.
    #[test]
    fn id_windows_decline_non_v7_ids() {
        assert!(id_windows("turn_abc123").is_empty());
        assert!(id_windows("").is_empty());
        assert!(id_windows(&uuid::Uuid::new_v4().to_string()).is_empty());
        assert!(id_windows("not-a-uuid-at-all-really").is_empty());
    }
}

#[cfg(test)]
mod sort_tests {
    use super::Sort;

    const ALLOWED: &[(&str, &str)] = &[("request_time", "num(ts_us)"), ("ttft_ms", "num(ttft_ms)")];

    #[test]
    fn sort_appends_a_deterministic_tie_break() {
        let s = Sort::new("request_time", "DESC", ALLOWED, &["num(ts_us)", "str(id)"]).unwrap();
        assert_eq!(s.keys, "-num(ts_us), -num(ts_us), -str(id)");
        let s = Sort::new("ttft_ms", "asc", ALLOWED, &["str(id)"]).unwrap();
        assert_eq!(s.keys, "+num(ttft_ms), +str(id)");
    }

    /// `sort_by` is spliced into the query, so anything off the whitelist has
    /// to be refused rather than passed through.
    #[test]
    fn sort_rejects_anything_off_the_whitelist() {
        for bad in ["", "id", "ts_us", "1) | delete", "num(ts_us)"] {
            assert!(
                Sort::new(bad, "DESC", ALLOWED, &[]).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    /// Anything that is not "asc" descends, matching the SQL backends.
    #[test]
    fn sort_order_defaults_to_descending() {
        assert!(Sort::new("request_time", "", ALLOWED, &[])
            .unwrap()
            .keys
            .starts_with('-'));
        assert!(Sort::new("request_time", "nonsense", ALLOWED, &[])
            .unwrap()
            .keys
            .starts_with('-'));
        assert!(Sort::new("request_time", "ASC", ALLOWED, &[])
            .unwrap()
            .keys
            .starts_with('+'));
    }
}
