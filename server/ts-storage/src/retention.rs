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

use ts_common::config::RetentionConfig;

use crate::backend::StorageBackend;

/// Granularity labels produced by `ts-metrics`. Mirrors
/// `server/ts-metrics/src/aggregator.rs` `GRANULARITIES`. Kept local to avoid
/// a dependency just for a 4-element constant; promote to `ts-common` if it
/// ever needs to be shared.
pub const KNOWN_METRICS_GRANULARITIES: &[&str] = &["10s", "1m", "5m", "1h"];

/// A concrete cutoff decision for each table. `None` means "skip".
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub calls_before: Option<SystemTime>,
    pub turns_before: Option<SystemTime>,
    /// `(granularity_label, cutoff)` pairs. Only listed granularities are swept.
    pub metrics_before: Vec<(String, SystemTime)>,
}

impl RetentionPolicy {
    /// True when no tables have a cutoff — used to short-circuit the sweep.
    pub fn is_empty(&self) -> bool {
        self.calls_before.is_none() && self.turns_before.is_none() && self.metrics_before.is_empty()
    }
}

/// Per-table row counts from a single sweep.
#[derive(Debug, Clone, Default)]
pub struct RetentionReport {
    pub calls_deleted: u64,
    pub turns_deleted: u64,
    /// Per-granularity deletion counts — one entry per swept label.
    pub metrics_deleted: HashMap<String, u64>,
}

impl RetentionReport {
    pub fn total(&self) -> u64 {
        self.calls_deleted + self.turns_deleted + self.metrics_deleted.values().sum::<u64>()
    }
}

/// Build a `RetentionPolicy` from config + the current wall clock.
///
/// Rules:
/// - Days value `0` or absent → no cutoff for that table.
/// - Unknown metrics granularity labels (not in `KNOWN_METRICS_GRANULARITIES`)
///   are logged at warn and skipped, to catch typos like `"10sec"` before they
///   silently retain data forever.
pub fn policy_from_config(cfg: &RetentionConfig, now: SystemTime) -> RetentionPolicy {
    let days_to_cutoff = |days: u32| -> Option<SystemTime> {
        if days == 0 {
            None
        } else {
            now.checked_sub(Duration::from_secs(u64::from(days) * 86_400))
        }
    };

    let mut metrics_before = Vec::new();
    for (label, days) in &cfg.metrics {
        if *days == 0 {
            continue;
        }
        if !KNOWN_METRICS_GRANULARITIES.contains(&label.as_str()) {
            warn!(
                granularity = label.as_str(),
                "retention: unknown metrics granularity in config; ignoring"
            );
            continue;
        }
        if let Some(cutoff) = days_to_cutoff(*days) {
            metrics_before.push((label.clone(), cutoff));
        }
    }

    RetentionPolicy {
        calls_before: days_to_cutoff(cfg.calls),
        turns_before: days_to_cutoff(cfg.turns),
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
                                    calls = report.calls_deleted,
                                    turns = report.turns_deleted,
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
    fn make_cfg(calls: u32, turns: u32, metrics: &[(&str, u32)]) -> RetentionConfig {
        let mut cfg = RetentionConfig::default();
        cfg.enabled = true;
        cfg.calls = calls;
        cfg.turns = turns;
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
        assert!(policy.calls_before.is_none());
        assert!(policy.turns_before.is_none());
        assert!(policy.metrics_before.is_empty());
        assert!(policy.is_empty());
    }

    #[test]
    fn seven_days_yields_cutoff_seven_days_ago() {
        let cfg = make_cfg(7, 30, &[]);
        let now = SystemTime::now();
        let policy = policy_from_config(&cfg, now);
        let calls_before = policy.calls_before.expect("calls cutoff");
        let elapsed = now.duration_since(calls_before).expect("cutoff before now");
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
    fn unknown_metrics_granularity_is_ignored() {
        let cfg = make_cfg(0, 0, &[("10sec", 1), ("1m", 7)]);
        let policy = policy_from_config(&cfg, SystemTime::now());
        // Only "1m" survives; "10sec" is dropped with a warn log.
        assert_eq!(policy.metrics_before.len(), 1);
        assert_eq!(policy.metrics_before[0].0, "1m");
    }

    #[test]
    fn zero_days_metrics_entry_is_skipped() {
        let cfg = make_cfg(0, 0, &[("10s", 0), ("1h", 365)]);
        let policy = policy_from_config(&cfg, SystemTime::now());
        assert_eq!(policy.metrics_before.len(), 1);
        assert_eq!(policy.metrics_before[0].0, "1h");
    }

    #[test]
    fn known_granularities_constant_matches_ts_metrics() {
        // Sanity check that this list stays in sync with the labels produced
        // by ts-metrics/src/aggregator.rs. If ts-metrics adds a new
        // granularity, update this constant.
        assert_eq!(KNOWN_METRICS_GRANULARITIES, &["10s", "1m", "5m", "1h"]);
    }
}
