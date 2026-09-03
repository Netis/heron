//! Retention, pushed to aglake as per-index TTLs.
//!
//! Every other backend implements `apply_retention` as a DELETE. aglake has no
//! DELETE: data leaves through bucket freezing, driven by a per-index
//! `frozen_after_secs` and a sweep timer inside aglaked. So this module does not
//! *delete* anything — it tells aglake what each index's TTL should be and lets
//! aglaked enforce it. The layout was chosen for exactly this: one index per
//! entity and per metrics granularity means Heron's per-table, per-granularity
//! retention maps one-to-one onto aglake's per-index knob, with nothing to
//! emulate.
//!
//! # Two consequences worth stating plainly
//!
//! **The report is always zero.** `RetentionReport` counts deleted rows, and
//! there are none to count: the push is a declaration, and the deletion happens
//! later, asynchronously, at bucket granularity. Reporting a fabricated number
//! would be worse than reporting none, so the sweep logs what it actually did —
//! which indexes got which TTL — and returns an empty report. The generic
//! retention loop treats that as "nothing deleted" and logs at debug, which is
//! the right volume for a call that is normally a no-op re-declaration.
//!
//! **Deletion is coarser than the cutoff.** A bucket survives until its
//! *newest* event ages out, so rows can outlive the policy by up to one
//! bucket's time span. Every backend's retention is approximate at the edges;
//! this one is approximate by a larger and more visible margin.
//!
//! # When the API is not reachable
//!
//! The native admin face is always mounted on a supported aglaked, so the
//! reachability question is now about version and credentials rather than how
//! the daemon was started: a 404 means it predates 0.3, a 401 that no session
//! was configured, a 403 that the account lacks the admin role (see
//! [`crate::client::ManagementClient`]). None of those is something Heron can
//! fix from here, so the failure is reported **once**, with what an operator
//! can actually do about it, and every sweep after that is a silent no-op —
//! while still retrying, because an aglaked restart or a re-login can make the
//! API appear.

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use h_common::error::Result;
use h_storage::retention::{RetentionPolicy, RetentionReport};

use crate::schema::Indexes;
use crate::AglakeBackend;

/// One index and the TTL it should be given, in seconds.
type Target = (String, u64);

/// Translate a policy into per-index TTLs.
///
/// Cutoffs come in as absolute instants; aglake wants a duration. The two are
/// the same statement made from opposite ends, so this is `now - cutoff` —
/// with the sign guarded, because a cutoff that is somehow in the future would
/// otherwise compute a TTL of zero, and zero means *freeze everything now*.
/// Getting that wrong once would destroy the data the policy was meant to
/// preserve, so a non-positive TTL drops the target instead.
fn plan(
    ix: &Indexes,
    policy: &RetentionPolicy,
    body_retention_days: u32,
    now: SystemTime,
) -> Vec<Target> {
    let ttl = |cutoff: SystemTime| -> Option<u64> {
        now.duration_since(cutoff)
            .ok()
            .map(|d| d.as_secs())
            .filter(|s| *s > 0)
    };
    // Bodies get their own TTL when configured, and otherwise inherit the
    // entity they belong to — bodies follow spans, HTTP bodies follow HTTP
    // exchanges. Inheriting is what makes a body outliving its metadata
    // impossible by default: an orphan body is unreachable, since every read
    // path finds bodies through their parent's id.
    let body_ttl = (body_retention_days > 0).then(|| u64::from(body_retention_days) * 86_400);

    let mut out: Vec<Target> = Vec::new();
    let mut push = |index: &str, secs: Option<u64>| {
        if let Some(s) = secs {
            out.push((index.to_string(), s));
        }
    };

    let spans = policy.spans_before.and_then(ttl);
    push(&ix.spans, spans);
    push(&ix.bodies, body_ttl.or(spans));

    push(&ix.traces, policy.traces_before.and_then(ttl));

    let http = policy.http_exchanges_before.and_then(ttl);
    push(&ix.http, http);
    push(&ix.http_bodies, body_ttl.or(http));

    for (label, cutoff) in &policy.metrics_before {
        let secs = ttl(*cutoff);
        if let Some(m) = ix.metrics_for(label) {
            push(m, secs);
        }
        if let Some(f) = ix.finish_for(label) {
            push(f, secs);
        }
    }
    out
}

impl AglakeBackend {
    pub(crate) async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        let report = RetentionReport::default();
        if !self.manage_retention {
            tracing::debug!(
                target: "aglake::retention",
                "aglake: storage.aglake.manage_retention is off; leaving retention to aglaked"
            );
            return Ok(report);
        }

        let targets = plan(
            &self.ix,
            &policy,
            self.body_retention_days,
            SystemTime::now(),
        );
        if targets.is_empty() {
            return Ok(report);
        }

        // One call that answers both "is this API mounted?" and "which of my
        // indexes exist yet?". Pushing to an index that has never been written
        // is a 404 that means "not yet" — expected on a fresh deployment, and
        // not something to report as a failure.
        let existing = match self.management.list_indexes().await {
            Ok(v) => v,
            Err(e) => {
                self.warn_retention_unavailable(&e);
                return Ok(report);
            }
        };

        let mut applied = 0usize;
        let mut absent = 0usize;
        let mut failed = 0usize;
        for (index, secs) in targets {
            let current = existing.iter().find(|i| i.name == index);
            let Some(current) = current else {
                absent += 1;
                continue;
            };
            // Re-declaring an unchanged TTL is a write on aglake's side, and
            // the sweep runs on a timer forever. Skip the ones already right.
            //
            // What the catalogue reports is the *effective* TTL, which may be
            // aglaked's server-wide `--retention-days` rather than a per-index
            // setting anybody pushed — the two are indistinguishable from
            // here. So this can decline to persist an explicit setting whose
            // value the server default happens to match. That leaves the
            // retention Heron wants in force either way, and if the server
            // default later moves, the next sweep sees the mismatch and
            // pushes. Self-healing within one interval is a fair price for
            // not rewriting thirteen indexes every hour.
            if current.frozen_after_secs == Some(secs as i64) {
                continue;
            }
            match self.management.set_retention(&index, secs).await {
                Ok(()) => {
                    applied += 1;
                    tracing::info!(
                        target: "aglake::retention",
                        index = %index, frozen_after_secs = secs,
                        "aglake: index retention updated"
                    );
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        target: "aglake::retention",
                        index = %index, error = %e,
                        "aglake: could not set index retention"
                    );
                }
            }
        }
        tracing::debug!(
            target: "aglake::retention",
            applied, absent, failed,
            "aglake: retention sweep complete (TTLs declared; aglaked deletes on its own timer)"
        );
        Ok(report)
    }

    /// Say it once, and say what to do about it. The retention loop runs on a
    /// timer, so an unconditional warning here would repeat forever for a
    /// condition that cannot change without an operator acting.
    fn warn_retention_unavailable(&self, e: &h_common::error::AppError) {
        if self.retention_warned.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                target: "aglake::retention",
                error = %e,
                "aglake: index management API still unreachable"
            );
            return;
        }
        tracing::warn!(
            target: "aglake::retention",
            error = %e,
            "aglake: cannot reach the index management API, so Heron's retention \
             policy is not being applied — data will be kept until aglaked's own \
             retention removes it. The error above says which of the three it \
             is: an aglaked too old to serve /api/v1/admin (404), no session \
             (401 — set storage.aglake.username / password), or an account \
             without the admin role (403). Or give aglaked a server-wide \
             --retention-days and set storage.aglake.manage_retention = false \
             to silence this."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ix() -> Indexes {
        Indexes::new("heron")
    }

    fn at(now: SystemTime, days: u64) -> SystemTime {
        now - Duration::from_secs(days * 86_400)
    }

    fn ttl_of(targets: &[Target], index: &str) -> Option<u64> {
        targets.iter().find(|(i, _)| i == index).map(|(_, s)| *s)
    }

    #[test]
    fn cutoffs_become_ttls_per_index() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let policy = RetentionPolicy {
            spans_before: Some(at(now, 7)),
            traces_before: Some(at(now, 30)),
            http_exchanges_before: Some(at(now, 3)),
            metrics_before: vec![("10s".into(), at(now, 1)), ("1h".into(), at(now, 365))],
        };
        let t = plan(&ix(), &policy, 0, now);

        assert_eq!(ttl_of(&t, "heron_spans"), Some(7 * 86_400));
        assert_eq!(ttl_of(&t, "heron_traces"), Some(30 * 86_400));
        assert_eq!(ttl_of(&t, "heron_http"), Some(3 * 86_400));
        assert_eq!(ttl_of(&t, "heron_metrics_10s"), Some(86_400));
        assert_eq!(ttl_of(&t, "heron_metrics_1h"), Some(365 * 86_400));
        // Finish metrics share their granularity's schedule.
        assert_eq!(ttl_of(&t, "heron_finish_10s"), Some(86_400));
        assert_eq!(ttl_of(&t, "heron_finish_1h"), Some(365 * 86_400));
    }

    /// An orphaned body is unreachable — every read finds bodies through the
    /// parent's id — so the default has to be that bodies never outlive it.
    #[test]
    fn bodies_inherit_their_own_parent_when_unset() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let policy = RetentionPolicy {
            spans_before: Some(at(now, 7)),
            http_exchanges_before: Some(at(now, 3)),
            ..Default::default()
        };
        let t = plan(&ix(), &policy, 0, now);
        assert_eq!(ttl_of(&t, "heron_bodies"), Some(7 * 86_400));
        assert_eq!(ttl_of(&t, "heron_http_bodies"), Some(3 * 86_400));

        let t = plan(&ix(), &policy, 2, now);
        assert_eq!(ttl_of(&t, "heron_bodies"), Some(2 * 86_400));
        assert_eq!(ttl_of(&t, "heron_http_bodies"), Some(2 * 86_400));
        // The parents keep their own schedule.
        assert_eq!(ttl_of(&t, "heron_spans"), Some(7 * 86_400));
    }

    /// `None` means "keep forever"; it must not fall through to some default.
    #[test]
    fn absent_cutoffs_produce_no_target() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let policy = RetentionPolicy {
            traces_before: Some(at(now, 30)),
            ..Default::default()
        };
        let t = plan(&ix(), &policy, 0, now);
        assert_eq!(t.len(), 1, "only traces has a cutoff: {t:?}");
        assert_eq!(ttl_of(&t, "heron_traces"), Some(30 * 86_400));
        assert_eq!(ttl_of(&t, "heron_bodies"), None);
    }

    /// aglake reads `frozen_after_secs = 0` as *freeze everything now*. A
    /// cutoff at or past the current instant must therefore drop the target
    /// rather than round down into a wipe.
    #[test]
    fn a_non_positive_ttl_is_dropped_not_sent_as_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let policy = RetentionPolicy {
            spans_before: Some(now),
            traces_before: Some(now + Duration::from_secs(3600)),
            ..Default::default()
        };
        let t = plan(&ix(), &policy, 0, now);
        assert!(t.is_empty(), "expected no targets, got {t:?}");
    }

    #[test]
    fn unknown_granularity_labels_are_skipped() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let policy = RetentionPolicy {
            metrics_before: vec![("7m".into(), at(now, 5))],
            ..Default::default()
        };
        assert!(plan(&ix(), &policy, 0, now).is_empty());
    }
}
