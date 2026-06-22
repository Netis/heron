//! Data retention: periodically deletes rows older than per-table cutoffs.
//!
//! `RetentionPolicy` is backend-neutral (timestamps + granularity labels); each
//! `StorageBackend` implements its own DELETE strategy. `spawn_retention_task`
//! drives a background loop that applies the policy on a fixed interval.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use h_common::config::RetentionConfig;

use crate::backend::StorageBackend;

/// A concrete cutoff decision for each table. `None` means "skip".
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub spans_before: Option<SystemTime>,
    pub traces_before: Option<SystemTime>,
    pub http_exchanges_before: Option<SystemTime>,
    /// `(granularity_label, cutoff)` pairs. Only listed granularities are swept.
    pub metrics_before: Vec<(String, SystemTime)>,
}

impl RetentionPolicy {
    /// True when no tables have a cutoff — used to short-circuit the sweep.
    pub fn is_empty(&self) -> bool {
        self.spans_before.is_none()
            && self.traces_before.is_none()
            && self.http_exchanges_before.is_none()
            && self.metrics_before.is_empty()
    }
}

/// Per-table row counts from a single sweep.
#[derive(Debug, Clone, Default)]
pub struct RetentionReport {
    pub spans_deleted: u64,
    pub traces_deleted: u64,
    pub http_exchanges_deleted: u64,
    /// Per-granularity deletion counts — one entry per swept label.
    pub metrics_deleted: HashMap<String, u64>,
}

impl RetentionReport {
    pub fn total(&self) -> u64 {
        self.spans_deleted
            + self.traces_deleted
            + self.http_exchanges_deleted
            + self.metrics_deleted.values().sum::<u64>()
    }
}

/// Build a `RetentionPolicy` from config + the current wall clock.
///
/// Rules:
/// - Days value `0` for `calls`/`turns`/`http_exchanges` → no cutoff for that table.
/// - `cfg.metrics` is read as-is. `RawAppConfig::resolve` (in h-common) has
///   already merged `DEFAULT_METRICS_RETENTION_DAYS` and dropped unknown
///   labels at load time, so this function trusts the map: every entry is a
///   known granularity, every known granularity has a value, and `0` means
///   "skip this granularity".
pub fn policy_from_config(cfg: &RetentionConfig, now: SystemTime) -> RetentionPolicy {
    let days_to_cutoff = |days: u32| -> Option<SystemTime> {
        if days == 0 {
            None
        } else {
            now.checked_sub(Duration::from_secs(u64::from(days) * 86_400))
        }
    };

    let metrics_before = cfg
        .metrics
        .iter()
        .filter_map(|(label, days)| days_to_cutoff(*days).map(|c| (label.clone(), c)))
        .collect();

    RetentionPolicy {
        spans_before: days_to_cutoff(cfg.spans),
        traces_before: days_to_cutoff(cfg.traces),
        http_exchanges_before: days_to_cutoff(cfg.http_exchanges),
        metrics_before,
    }
}

/// Spawn the retention background loop. Exits when `cancel` fires.
///
/// Returns immediately (with a completed-style handle) when retention is
/// disabled or the config produces an empty policy — the caller always gets a
/// `JoinHandle` it can `.await` without special-casing.
pub fn spawn_retention_task(
    backend: Arc<dyn StorageBackend>,
    cfg: RetentionConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            debug!("retention: disabled in config; task exiting");
            return;
        }
        let probe = policy_from_config(&cfg, SystemTime::now());
        if probe.is_empty() {
            info!("retention: enabled but no cutoffs configured; task exiting");
            return;
        }

        let interval_secs = cfg.check_interval_secs.max(1);
        info!(
            interval_secs,
            "retention: task started (per-table / per-granularity TTL)"
        );
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // First tick fires immediately; consume it so we don't double-sweep.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("retention: cancellation received; task exiting");
                    return;
                }
                _ = ticker.tick() => {
                    let policy = policy_from_config(&cfg, SystemTime::now());
                    if policy.is_empty() {
                        continue;
                    }
                    match backend.apply_retention(policy).await {
                        Ok(report) => {
                            if report.total() > 0 {
                                info!(
                                    calls = report.spans_deleted,
                                    turns = report.traces_deleted,
                                    http_exchanges = report.http_exchanges_deleted,
                                    metrics = ?report.metrics_deleted,
                                    "retention: sweep complete"
                                );
                            } else {
                                debug!("retention: sweep complete (no rows deleted)");
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "retention: sweep failed");
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `RetentionConfig` for tests that exercise *policy* shape
    /// (cutoffs, skip semantics). Default-merge / unknown-label handling is
    /// the load layer's job — those tests live in
    /// `h-common::config::phase2_tests`.
    fn make_cfg(calls: u32, turns: u32, metrics: &[(&str, u32)]) -> RetentionConfig {
        let mut cfg = RetentionConfig::default();
        cfg.enabled = true;
        cfg.spans = calls;
        cfg.traces = turns;
        cfg.http_exchanges = 0;
        cfg.metrics = metrics
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect();
        cfg
    }

    #[test]
    fn zero_days_means_no_cutoff() {
        let cfg = make_cfg(0, 0, &[]);
        let now = SystemTime::now();
        let policy = policy_from_config(&cfg, now);
        assert!(policy.spans_before.is_none());
        assert!(policy.traces_before.is_none());
        assert!(policy.http_exchanges_before.is_none());
        assert!(policy.metrics_before.is_empty());
        assert!(policy.is_empty());
    }

    #[test]
    fn http_exchanges_cutoff_applied_when_days_positive() {
        let mut cfg = make_cfg(0, 0, &[]);
        cfg.http_exchanges = 7;
        let now = SystemTime::now();
        let policy = policy_from_config(&cfg, now);
        let cutoff = policy.http_exchanges_before.expect("http_exchanges cutoff");
        let elapsed = now.duration_since(cutoff).expect("cutoff before now");
        let expected = Duration::from_secs(7 * 86_400);
        let delta = if elapsed > expected {
            elapsed - expected
        } else {
            expected - elapsed
        };
        assert!(delta < Duration::from_secs(1), "delta = {delta:?}");
        assert!(!policy.is_empty());
    }

    #[test]
    fn default_retention_config_has_seven_day_http_exchanges() {
        let cfg = RetentionConfig::default();
        assert_eq!(cfg.http_exchanges, 7);
    }

    #[test]
    fn seven_days_yields_cutoff_seven_days_ago() {
        let cfg = make_cfg(7, 30, &[]);
        let now = SystemTime::now();
        let policy = policy_from_config(&cfg, now);
        let spans_before = policy.spans_before.expect("calls cutoff");
        let elapsed = now.duration_since(spans_before).expect("cutoff before now");
        // Within 1 second of exactly 7*86400 seconds.
        let expected = Duration::from_secs(7 * 86_400);
        let delta = if elapsed > expected {
            elapsed - expected
        } else {
            expected - elapsed
        };
        assert!(delta < Duration::from_secs(1), "delta = {delta:?}");
    }

    #[test]
    fn zero_days_metrics_entry_is_skipped() {
        let cfg = make_cfg(0, 0, &[("10s", 0), ("1h", 365)]);
        let policy = policy_from_config(&cfg, SystemTime::now());
        assert_eq!(policy.metrics_before.len(), 1);
        assert_eq!(policy.metrics_before[0].0, "1h");
    }
}
