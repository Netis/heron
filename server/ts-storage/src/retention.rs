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

/// Default per-granularity retention for `llm_metrics`, in days.
///
/// Doubles as the source of truth for **known granularity labels** — the keys
/// must match the labels produced by `ts-metrics::aggregator::GRANULARITIES`.
/// `policy_from_config` merges user overrides on top of this table, so adding
/// a new granularity only requires updating ts-metrics + this single constant.
pub const DEFAULT_METRICS_RETENTION_DAYS: &[(&str, u32)] = &[
    ("10s", 1),
    ("1m", 7),
    ("5m", 30),
    ("1h", 365),
];

/// A concrete cutoff decision for each table. `None` means "skip".
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub calls_before: Option<SystemTime>,
    pub turns_before: Option<SystemTime>,
    pub http_exchanges_before: Option<SystemTime>,
    /// `(granularity_label, cutoff)` pairs. Only listed granularities are swept.
    pub metrics_before: Vec<(String, SystemTime)>,
}

impl RetentionPolicy {
    /// True when no tables have a cutoff — used to short-circuit the sweep.
    pub fn is_empty(&self) -> bool {
        self.calls_before.is_none()
            && self.turns_before.is_none()
            && self.http_exchanges_before.is_none()
            && self.metrics_before.is_empty()
    }
}

/// Per-table row counts from a single sweep.
#[derive(Debug, Clone, Default)]
pub struct RetentionReport {
    pub calls_deleted: u64,
    pub turns_deleted: u64,
    pub http_exchanges_deleted: u64,
    /// Per-granularity deletion counts — one entry per swept label.
    pub metrics_deleted: HashMap<String, u64>,
}

impl RetentionReport {
    pub fn total(&self) -> u64 {
        self.calls_deleted
            + self.turns_deleted
            + self.http_exchanges_deleted
            + self.metrics_deleted.values().sum::<u64>()
    }
}

/// Build a `RetentionPolicy` from config + the current wall clock.
///
/// Rules:
/// - Days value `0` for `calls`/`turns`/`http_exchanges` → no cutoff for that table.
/// - For each granularity in `DEFAULT_METRICS_RETENTION_DAYS`, the user's
///   override (if any) wins; otherwise the default applies. `0` disables that
///   granularity. This means setting only `"1h" = 730` keeps the other three
///   defaults intact.
/// - Unknown metrics granularity labels in user config (not in
///   `DEFAULT_METRICS_RETENTION_DAYS`) are logged at warn and ignored, to catch
///   typos like `"10sec"` before they silently retain data forever.
pub fn policy_from_config(cfg: &RetentionConfig, now: SystemTime) -> RetentionPolicy {
    let days_to_cutoff = |days: u32| -> Option<SystemTime> {
        if days == 0 {
            None
        } else {
            now.checked_sub(Duration::from_secs(u64::from(days) * 86_400))
        }
    };

    for label in cfg.metrics.keys() {
        if !DEFAULT_METRICS_RETENTION_DAYS
            .iter()
            .any(|(known, _)| known == label)
        {
            warn!(
                granularity = label.as_str(),
                "retention: unknown metrics granularity in config; ignoring"
            );
        }
    }

    let mut metrics_before = Vec::new();
    for (label, default_days) in DEFAULT_METRICS_RETENTION_DAYS {
        let days = cfg.metrics.get(*label).copied().unwrap_or(*default_days);
        if let Some(cutoff) = days_to_cutoff(days) {
            metrics_before.push(((*label).to_string(), cutoff));
        }
    }

    RetentionPolicy {
        calls_before: days_to_cutoff(cfg.calls),
        turns_before: days_to_cutoff(cfg.turns),
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
                                    calls = report.calls_deleted,
                                    turns = report.turns_deleted,
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
    fn make_cfg(calls: u32, turns: u32, metrics: &[(&str, u32)]) -> RetentionConfig {
        let mut cfg = RetentionConfig::default();
        cfg.enabled = true;
        cfg.calls = calls;
        cfg.turns = turns;
        // Start every test from a clean slate so policy-shape assertions
        // (is_empty, single-entry checks) stay meaningful: zero out
        // http_exchanges and every known metrics granularity, then overlay
        // the test's explicit overrides on top. Defaults are exercised in
        // dedicated cases below.
        cfg.http_exchanges = 0;
        cfg.metrics = DEFAULT_METRICS_RETENTION_DAYS
            .iter()
            .map(|(label, _)| ((*label).to_string(), 0))
            .collect();
        for (k, v) in metrics {
            cfg.metrics.insert((*k).to_string(), *v);
        }
        cfg
    }

    #[test]
    fn zero_days_means_no_cutoff() {
        let cfg = make_cfg(0, 0, &[]);
        let now = SystemTime::now();
        let policy = policy_from_config(&cfg, now);
        assert!(policy.calls_before.is_none());
        assert!(policy.turns_before.is_none());
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
    fn default_granularity_table_matches_ts_metrics() {
        // Sanity check that the default table stays in sync with the labels
        // produced by ts-metrics/src/aggregator.rs. If ts-metrics adds a new
        // granularity, update this constant — the default-day value is the
        // recommended fresh-install retention for that label.
        assert_eq!(
            DEFAULT_METRICS_RETENTION_DAYS,
            &[("10s", 1u32), ("1m", 7), ("5m", 30), ("1h", 365)]
        );
    }

    #[test]
    fn missing_metrics_keys_get_default_retention() {
        // Empty user override → every known granularity gets its default cutoff.
        let mut cfg = RetentionConfig::default();
        cfg.metrics.clear();
        let policy = policy_from_config(&cfg, SystemTime::now());
        assert_eq!(policy.metrics_before.len(), DEFAULT_METRICS_RETENTION_DAYS.len());
    }

    #[test]
    fn user_override_for_one_granularity_keeps_other_defaults() {
        // The whole reason for default-merge: overriding "1h" must not silently
        // drop retention for the other three labels.
        let mut cfg = RetentionConfig::default();
        cfg.metrics.clear();
        cfg.metrics.insert("1h".to_string(), 730);
        let policy = policy_from_config(&cfg, SystemTime::now());
        let labels: Vec<&str> = policy.metrics_before.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"10s"));
        assert!(labels.contains(&"1m"));
        assert!(labels.contains(&"5m"));
        assert!(labels.contains(&"1h"));
    }
}
