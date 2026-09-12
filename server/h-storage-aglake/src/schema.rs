//! Index naming and `init()`.
//!
//! aglake has no DDL: an index springs into existence on first write, and
//! fields are extracted at search time. So "schema" here is two things —
//! deciding which index each entity lands in, and checking at startup that the
//! deployment is configured well enough to be fast.
//!
//! # Why this many indexes
//!
//! **Bodies are separate from metadata.** List and aggregate queries never
//! need a request/response body, and a body is three orders of magnitude
//! larger than the columns beside it. Splitting them means those queries never
//! decompress body bytes, and `include_bodies = false` becomes "don't run the
//! second query" rather than "project the column away". It also lets bodies
//! expire on their own schedule.
//!
//! **Granularity is part of the index name, not a field.** Retention in aglake
//! is per-index, while Heron's metrics retention is per-granularity (10s for a
//! day, 1h for a year). Encoding the granularity in the name makes that a
//! direct mapping instead of something the backend has to emulate, and turns
//! the most common metrics filter into index-level pruning.
//!
//! **One sourcetype per index.** The columnar fast path requires every
//! sourcetype in a bucket to have indexed the field being read
//! (`bucket_fully_indexes`, `aglake-exec/src/run.rs`). Two sourcetypes with
//! different `indexed` sets in one index would knock every query in that
//! bucket back onto the row path. That is why finish-metric rows get their own
//! index rather than sharing with the wide metric rows.

use h_common::config::DEFAULT_METRICS_RETENTION_DAYS;
use h_common::error::Result;

use crate::AglakeBackend;

/// Resolved index names for one `index_prefix`.
#[derive(Debug, Clone)]
pub struct Indexes {
    pub spans: String,
    pub bodies: String,
    pub traces: String,
    pub http: String,
    pub http_bodies: String,
    /// `(granularity label, index name)`, one per known granularity.
    pub metrics: Vec<(String, String)>,
    pub finish: Vec<(String, String)>,
}

impl Indexes {
    pub fn new(prefix: &str) -> Self {
        let g = |kind: &str| -> Vec<(String, String)> {
            DEFAULT_METRICS_RETENTION_DAYS
                .iter()
                .map(|(label, _)| ((*label).to_string(), format!("{prefix}_{kind}_{label}")))
                .collect()
        };
        Self {
            spans: format!("{prefix}_spans"),
            bodies: format!("{prefix}_bodies"),
            traces: format!("{prefix}_traces"),
            http: format!("{prefix}_http"),
            http_bodies: format!("{prefix}_http_bodies"),
            metrics: g("metrics"),
            finish: g("finish"),
        }
    }

    /// Metrics index for a granularity label, or `None` if the label is not one
    /// of the known granularities.
    pub fn metrics_for(&self, granularity: &str) -> Option<&str> {
        self.metrics
            .iter()
            .find(|(l, _)| l == granularity)
            .map(|(_, ix)| ix.as_str())
    }

    /// Finish-metrics index for a granularity label.
    pub fn finish_for(&self, granularity: &str) -> Option<&str> {
        self.finish
            .iter()
            .find(|(l, _)| l == granularity)
            .map(|(_, ix)| ix.as_str())
    }

    /// Every index this backend owns.
    pub fn all(&self) -> Vec<&str> {
        let mut v = vec![
            self.spans.as_str(),
            self.bodies.as_str(),
            self.traces.as_str(),
            self.http.as_str(),
            self.http_bodies.as_str(),
        ];
        v.extend(self.metrics.iter().map(|(_, ix)| ix.as_str()));
        v.extend(self.finish.iter().map(|(_, ix)| ix.as_str()));
        v
    }
}

/// aglake's built-in index names. Colliding with one of these would mix Heron
/// data into someone else's — `traces` in particular already holds OTLP spans,
/// including the ones aglaked writes about its own searches.
pub const RESERVED_INDEXES: &[&str] = &[
    "main",
    "summary",
    "metrics",
    "traces",
    "_internal",
    "_audit",
    "_introspection",
    "_metrics",
];

impl AglakeBackend {
    /// Best-effort check for whether write-time field extraction is in place.
    ///
    /// `tstats` reads postings only and can therefore only see fields listed
    /// under `indexed` in props.toml — so a `tstats` that returns rows proves
    /// the configuration took effect. An empty index proves nothing either
    /// way, so that case reports "configured" to avoid a misleading warning on
    /// a fresh deployment.
    pub(crate) async fn probe_indexed_fields(&self) -> Result<bool> {
        let ix = &self.ix.spans;
        let count = self
            .search
            .search_all_time(&format!("search index={ix} | stats count as n | table n"))
            .await?
            .rows()
            .first()
            .and_then(|r| r.get("n"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if count == 0 {
            return Ok(true); // nothing ingested yet — cannot tell, do not warn
        }
        Ok(
            match self
                .search
                .search_all_time(&format!("| tstats count where index={ix} by wire_api"))
                .await
            {
                Ok(r) => !r.rows().is_empty(),
                Err(_) => false,
            },
        )
    }
}

/// Startup check. There is nothing to create — indexes appear on first write —
/// so this verifies reachability and reports anything that would make the
/// deployment quietly slow.
pub(crate) async fn init(backend: &AglakeBackend) -> Result<()> {
    backend.search.ping().await?;

    // `indexed` is what keeps aggregates off the row path. It is loaded once
    // at aglaked startup and never applied retroactively, so we can only report
    // — rewriting someone else's props.toml from here would be both surprising
    // and useless for data already on disk.
    if !backend.probe_indexed_fields().await? {
        tracing::warn!(
            target: "aglake::props",
            "aglake: write-time field extraction does not appear to be configured. \
             Queries stay correct but aggregates fall back to decompressing bodies. \
             Merge the stanzas from `heron aglake-props` into aglaked's \
             <data-dir>/props.toml and restart aglaked; note index-time config \
             only applies to newly ingested data."
        );
    }

    tracing::info!(
        target: "aglake",
        indexes = backend.ix.all().len(),
        prefix_spans = %backend.ix.spans,
        "aglake storage backend initialized"
    );

    // Say this at startup rather than leaving an operator to discover an empty
    // proxy view and wonder which layer dropped it.
    if backend.enable_trace_patching {
        tracing::warn!(
            target: "aglake",
            "aglake: storage.aglake.enable_trace_patching is set but not \
             implemented on this backend, and is being ignored. Behaviour is \
             the same as leaving it off."
        );
    }
    tracing::info!(
        target: "aglake",
        "aglake: traces are append-only, so proxy pairing does not annotate \
         them — proxy_role / proxy_peer_turn_id stay unset and the topology \
         graph has no proxy edges. Emulating updates would put every traces \
         read behind a full-window sort, which is what the pagination design \
         exists to avoid."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_names_avoid_reserved_collisions() {
        let ix = Indexes::new("heron");
        for name in ix.all() {
            assert!(
                !RESERVED_INDEXES.contains(&name),
                "{name} collides with a aglake built-in index"
            );
        }
    }

    #[test]
    fn one_index_per_granularity_for_both_metric_shapes() {
        let ix = Indexes::new("heron");
        assert_eq!(ix.metrics.len(), DEFAULT_METRICS_RETENTION_DAYS.len());
        assert_eq!(ix.finish.len(), DEFAULT_METRICS_RETENTION_DAYS.len());
        assert_eq!(ix.metrics_for("10s"), Some("heron_metrics_10s"));
        assert_eq!(ix.finish_for("1h"), Some("heron_finish_1h"));
        assert_eq!(ix.metrics_for("10sec"), None);
    }

    /// Wide rows and finish rows must not share an index: differing `indexed`
    /// sets in one bucket disable the columnar fast path for the whole bucket.
    #[test]
    fn metric_and_finish_indexes_are_disjoint() {
        let ix = Indexes::new("heron");
        for (_, m) in &ix.metrics {
            for (_, f) in &ix.finish {
                assert_ne!(m, f);
            }
        }
    }

    #[test]
    fn all_index_names_are_unique() {
        let ix = Indexes::new("heron");
        let mut names = ix.all();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate index name");
    }
}
