//! File retention for `pcap_dump` output.
//!
//! Walks `<dump_dir>/<source_id>/<minute_label>.pcap[.snappy]` on a fixed
//! interval and deletes files that fall outside the configured age / size
//! limits. Designed to be safe alongside the active [`PacketDumper`]: the
//! current and previous wall-clock minutes are always preserved so we
//! never race the writer.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use h_common::config::PcapDumpRetentionConfig;
use h_common::internal_metrics::{Metric, MetricsWorker};
use h_common::throttle::ThrottledWarn;

use crate::pcap_dump::parse_minute_label;

const WARN_THROTTLE: Duration = Duration::from_secs(5);
const MIB: u64 = 1024 * 1024;

/// Per-tick result, surfaced via metrics + logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub files_deleted: u64,
    pub bytes_deleted: u64,
    pub errors: u64,
}

impl SweepReport {
    pub fn is_noop(&self) -> bool {
        self.files_deleted == 0 && self.bytes_deleted == 0 && self.errors == 0
    }
}

/// One discovered file eligible for deletion (i.e. not race-protected).
struct Candidate {
    path: PathBuf,
    minute_key: i64,
    size: u64,
}

/// Outcome of one `try_unlink` call. Each pipeline's retention task
/// scopes itself to `<dir>/<pipeline>/...`, so well-behaved deployments
/// won't see a sibling task racing this one. `AlreadyGone` exists for
/// the *unexpected* races: an operator manually `rm`'d a file between
/// our `read_dir` walk and the `remove_file` call, an external cleanup
/// script ran in parallel, or a misconfigured deploy points two
/// retention tasks at overlapping subtrees. In all those cases ENOENT
/// is not an error — we just shouldn't claim credit for bytes we didn't
/// reclaim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnlinkOutcome {
    /// We unlinked the file.
    Removed,
    /// `ENOENT` — file was already gone (manual rm, external cleanup,
    /// or an unexpected concurrent retention task).
    AlreadyGone,
    /// Real failure (EPERM, EBUSY, etc.). `report.errors` already bumped.
    Failed,
}

/// Run one cleanup sweep. Pure function over the file system + an injected
/// `now` so it's deterministic in tests. Returns counts; the caller is
/// responsible for bumping metrics and logging.
pub fn run_sweep(dump_dir: &Path, cfg: &PcapDumpRetentionConfig, now: SystemTime) -> SweepReport {
    let mut report = SweepReport::default();
    let mut warner = ThrottledWarn::new(WARN_THROTTLE);

    let now_minute = system_time_to_minute_key(now);
    // Race protection: never touch the current wall-clock minute or the
    // previous one. The active writer rotates on minute boundary; after
    // crossing into minute N+1, file N is closed and flushed (heartbeat
    // every ~1s + flush on shutdown). Keeping the previous minute too is
    // a 1-minute safety margin for any in-flight buffered bytes.
    let race_cutoff = now_minute - 1;

    let entries = match fs::read_dir(dump_dir) {
        Ok(e) => e,
        Err(e) => {
            // Missing dir is fine — pcap_dump may not have produced any
            // output yet; treat anything else as a transient warn.
            if e.kind() != std::io::ErrorKind::NotFound {
                warn_throttled(
                    &mut warner,
                    &format!(
                        "pcap-retention: read_dir '{}' failed: {e}",
                        dump_dir.display()
                    ),
                );
                report.errors += 1;
            }
            return report;
        }
    };

    let mut candidates: Vec<Candidate> = Vec::new();
    // Explicit `for entry in entries` so per-entry `Err` (e.g. EACCES on
    // a single file's stat during readdir) is counted + warned rather
    // than silently dropped by `.flatten()`. Same for the inner loop.
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn_throttled(
                    &mut warner,
                    &format!(
                        "pcap-retention: dirent error in '{}': {e}",
                        dump_dir.display()
                    ),
                );
                report.errors += 1;
                continue;
            }
        };
        let src_dir = entry.path();
        if !src_dir.is_dir() {
            continue;
        }
        let inner = match fs::read_dir(&src_dir) {
            Ok(e) => e,
            Err(e) => {
                warn_throttled(
                    &mut warner,
                    &format!(
                        "pcap-retention: read_dir '{}' failed: {e}",
                        src_dir.display()
                    ),
                );
                report.errors += 1;
                continue;
            }
        };
        for f in inner {
            let f = match f {
                Ok(e) => e,
                Err(e) => {
                    warn_throttled(
                        &mut warner,
                        &format!(
                            "pcap-retention: dirent error in '{}': {e}",
                            src_dir.display()
                        ),
                    );
                    report.errors += 1;
                    continue;
                }
            };
            let path = f.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(minute_key) = parse_dump_filename(name) else {
                // Foreign file — leave it alone. We only reap what we created.
                continue;
            };
            if minute_key >= race_cutoff {
                continue;
            }
            let size = match f.metadata() {
                Ok(m) => m.len(),
                Err(e) => {
                    warn_throttled(
                        &mut warner,
                        &format!("pcap-retention: stat '{}' failed: {e}", path.display()),
                    );
                    report.errors += 1;
                    continue;
                }
            };
            candidates.push(Candidate {
                path,
                minute_key,
                size,
            });
        }
    }

    // Phase 1: age sweep. Files whose unlink genuinely fails (transient
    // EPERM, EBUSY, etc.) are kept in `candidates` so phase 2's `total`
    // accounts for the bytes still on disk — otherwise a single age-phase
    // failure would silently undersize the cap and skip an eviction we
    // should make. The error is already counted + throttled-warned by
    // `try_unlink`. ENOENT (file already gone — sharing-dir race, manual
    // rm) is silently dropped from candidates without bumping counters.
    if cfg.max_age_hours > 0 {
        let age_cutoff_minute = now_minute - i64::from(cfg.max_age_hours) * 60;
        candidates.retain(|c| {
            if c.minute_key >= age_cutoff_minute {
                return true;
            }
            match try_unlink(&c.path, &mut warner, &mut report) {
                UnlinkOutcome::Removed => {
                    report.files_deleted += 1;
                    report.bytes_deleted += c.size;
                    false
                }
                UnlinkOutcome::AlreadyGone => false,
                UnlinkOutcome::Failed => true,
            }
        });
    }

    // Phase 2: size sweep. ENOENT here means a file we listed at walk
    // time has since disappeared (external `rm`, admin cleanup, etc.).
    // Treat as "size accounted for" — decrement `current` so we don't
    // over-evict, but don't claim credit for bytes we didn't reclaim.
    if cfg.max_size_mb > 0 {
        let cap_bytes = cfg.max_size_mb.saturating_mul(MIB);
        let total: u64 = candidates.iter().map(|c| c.size).sum();
        if total > cap_bytes {
            // Oldest first. Stable secondary by path for deterministic tests.
            candidates.sort_by(|a, b| {
                a.minute_key
                    .cmp(&b.minute_key)
                    .then_with(|| a.path.cmp(&b.path))
            });
            let mut current = total;
            for c in &candidates {
                if current <= cap_bytes {
                    break;
                }
                match try_unlink(&c.path, &mut warner, &mut report) {
                    UnlinkOutcome::Removed => {
                        report.files_deleted += 1;
                        report.bytes_deleted += c.size;
                        current = current.saturating_sub(c.size);
                    }
                    UnlinkOutcome::AlreadyGone => {
                        current = current.saturating_sub(c.size);
                    }
                    UnlinkOutcome::Failed => {}
                }
            }
        }
    }

    // Source dirs are intentionally left in place even when emptied.
    // `pcap_dump::PacketDumper` caches a `SourceWriter` per source on
    // first packet and assumes the dir exists for all future minute
    // rotations; pruning here would race with that assumption (idle
    // source whose dir we delete → next packet fails to open the minute
    // file → source disabled until restart).

    report
}

/// Match `<minute_label>.pcap` and `<minute_label>.pcap.snappy`. Returns the
/// parsed `minute_key` (inverse of the label format) or `None` for any other
/// shape — protects foreign files from accidental deletion.
fn parse_dump_filename(name: &str) -> Option<i64> {
    let stem = name
        .strip_suffix(".pcap.snappy")
        .or_else(|| name.strip_suffix(".pcap"))?;
    parse_minute_label(stem)
}

fn system_time_to_minute_key(now: SystemTime) -> i64 {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    secs.div_euclid(60)
}

fn try_unlink(path: &Path, warner: &mut ThrottledWarn, report: &mut SweepReport) -> UnlinkOutcome {
    match fs::remove_file(path) {
        Ok(()) => UnlinkOutcome::Removed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UnlinkOutcome::AlreadyGone,
        Err(e) => {
            warn_throttled(
                warner,
                &format!("pcap-retention: unlink '{}' failed: {e}", path.display()),
            );
            report.errors += 1;
            UnlinkOutcome::Failed
        }
    }
}

fn warn_throttled(warner: &mut ThrottledWarn, msg: &str) {
    if let Some(suppressed) = warner.tick() {
        if suppressed > 0 {
            warn!(suppressed, "{msg} (latest of many)");
        } else {
            warn!("{msg}");
        }
    }
}

/// Spawn the pcap-dump retention loop. Mirrors
/// [`h_storage::spawn_retention_task`]: returns immediately when retention
/// is disabled or has no rules, exits cleanly on cancellation.
pub fn spawn_pcap_retention_task(
    dump_dir: PathBuf,
    cfg: PcapDumpRetentionConfig,
    metrics: MetricsWorker,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            debug!(dir = %dump_dir.display(), "pcap-retention: disabled in config; task exiting");
            return;
        }
        if cfg.is_empty() {
            info!(
                dir = %dump_dir.display(),
                "pcap-retention: enabled but max_age_hours and max_size_mb are both 0; task exiting"
            );
            return;
        }

        // Clamp to ≥1s so a misconfigured `check_interval_secs = 0` doesn't
        // turn into a CPU-burning sweep loop. Mirrors `h_storage::spawn_retention_task`.
        let interval_secs = cfg.check_interval_secs.max(1);
        info!(
            dir = %dump_dir.display(),
            interval_secs,
            max_age_hours = cfg.max_age_hours,
            max_size_mb = cfg.max_size_mb,
            "pcap-retention: task started"
        );
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // consume immediate tick

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!(dir = %dump_dir.display(), "pcap-retention: cancellation received; task exiting");
                    return;
                }
                _ = ticker.tick() => {
                    // Sweep walks the dump tree synchronously (`std::fs`
                    // read_dir / metadata / remove_file). Offload to a
                    // blocking thread so a large tree doesn't stall a
                    // Tokio worker — the runtime keeps responding to
                    // cancellation, ticks, and other tasks while the OS
                    // chews through the directory.
                    let dir_for_sweep = dump_dir.clone();
                    let cfg_for_sweep = cfg.clone();
                    let report = match tokio::task::spawn_blocking(move || {
                        run_sweep(&dir_for_sweep, &cfg_for_sweep, SystemTime::now())
                    })
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(
                                dir = %dump_dir.display(),
                                error = %e,
                                "pcap-retention: sweep task join failed; counted as one error"
                            );
                            SweepReport {
                                errors: 1,
                                ..SweepReport::default()
                            }
                        }
                    };
                    metrics
                        .counter(Metric::CaptureDumpRetentionFilesDeleted)
                        .add(report.files_deleted);
                    metrics
                        .counter(Metric::CaptureDumpRetentionBytesDeleted)
                        .add(report.bytes_deleted);
                    metrics
                        .counter(Metric::CaptureDumpRetentionErrors)
                        .add(report.errors);
                    if !report.is_noop() {
                        info!(
                            dir = %dump_dir.display(),
                            files_deleted = report.files_deleted,
                            bytes_deleted = report.bytes_deleted,
                            errors = report.errors,
                            "pcap-retention: sweep complete"
                        );
                    } else {
                        debug!(dir = %dump_dir.display(), "pcap-retention: sweep complete (no-op)");
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Test helper: create a minute file with `bytes` zero bytes inside.
    fn touch(dir: &Path, source: &str, minute_key: i64, suffix: &str, bytes: u64) -> PathBuf {
        let src_dir = dir.join(source);
        fs::create_dir_all(&src_dir).unwrap();
        let label = crate::pcap_dump::minute_label(minute_key);
        let path = src_dir.join(format!("{label}{suffix}"));
        let mut f = fs::File::create(&path).unwrap();
        if bytes > 0 {
            f.write_all(&vec![0u8; bytes as usize]).unwrap();
        }
        path
    }

    fn now_at_minute(minute_key: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs((minute_key * 60) as u64)
    }

    fn cfg(max_age_hours: u32, max_size_mb: u64) -> PcapDumpRetentionConfig {
        PcapDumpRetentionConfig {
            enabled: true,
            check_interval_secs: 3600,
            max_age_hours,
            max_size_mb,
        }
    }

    #[test]
    fn age_sweep_deletes_files_older_than_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        // current = 1000 minutes; max_age = 1h = 60 minutes; cutoff = 940.
        let p_old = touch(dir.path(), "s", 900, ".pcap", 100); // older than cutoff
        let p_edge = touch(dir.path(), "s", 940, ".pcap", 100); // exactly cutoff (kept: < strict)
        let p_new = touch(dir.path(), "s", 980, ".pcap", 100); // newer than cutoff

        let report = run_sweep(dir.path(), &cfg(1, 0), now_at_minute(1000));

        assert_eq!(report.files_deleted, 1);
        assert_eq!(report.bytes_deleted, 100);
        assert!(!p_old.exists(), "old file must be deleted");
        assert!(p_edge.exists(), "edge file must be kept");
        assert!(p_new.exists(), "new file must be kept");
    }

    #[test]
    fn race_protection_keeps_current_and_previous_minute() {
        // current = 1000 → race-protected minute_keys: 999, 1000.
        // Force the size sweep to want to delete everything (cap=1 MiB,
        // total eligible 4 MiB after race protection of 999+1000) and
        // confirm 999/1000 survive.
        let dir = tempfile::tempdir().unwrap();
        let p_curr = touch(dir.path(), "s", 1000, ".pcap", 2 * MIB);
        let p_prev = touch(dir.path(), "s", 999, ".pcap", 2 * MIB);
        let p_998 = touch(dir.path(), "s", 998, ".pcap", 2 * MIB);
        let p_997 = touch(dir.path(), "s", 997, ".pcap", 2 * MIB);

        let report = run_sweep(dir.path(), &cfg(0, 1), now_at_minute(1000));

        assert!(p_curr.exists(), "current minute must never be deleted");
        assert!(p_prev.exists(), "previous minute must never be deleted");
        assert!(!p_997.exists(), "minute 997 (outside race window) deleted");
        assert!(!p_998.exists(), "minute 998 (outside race window) deleted");
        assert_eq!(report.files_deleted, 2);
        assert_eq!(report.bytes_deleted, 4 * MIB);
    }

    #[test]
    fn race_protection_holds_even_when_size_cap_demands_more() {
        // Cap is so tight that to satisfy it we'd need to delete the
        // race-protected minute too. The sweep deletes what it can and
        // stops; the contract is "respect the writer", not "always meet
        // the cap". Total 4 MiB, cap 1 MiB. Eligible is only minute 998
        // (2 MiB). After deleting it, 2 MiB remain — still > cap, but
        // the protected files are off-limits.
        let dir = tempfile::tempdir().unwrap();
        let p_curr = touch(dir.path(), "s", 1000, ".pcap", 2 * MIB);
        let p_prev = touch(dir.path(), "s", 999, ".pcap", 2 * MIB);
        let p_old = touch(dir.path(), "s", 998, ".pcap", 2 * MIB);

        let report = run_sweep(dir.path(), &cfg(0, 1), now_at_minute(1000));

        assert!(p_curr.exists());
        assert!(p_prev.exists());
        assert!(!p_old.exists());
        assert_eq!(report.files_deleted, 1);
        assert_eq!(report.bytes_deleted, 2 * MIB);
    }

    #[test]
    fn size_sweep_evicts_oldest_first_across_sources() {
        let dir = tempfile::tempdir().unwrap();
        // current = 1000. Files 100..900 across two sources, eligible.
        // 9 files * 1 MiB each = 9 MiB. Cap = 5 MiB → must delete 4 oldest.
        let mut paths_by_age: Vec<(i64, PathBuf)> = Vec::new();
        for (i, k) in [100, 200, 300, 400, 500, 600, 700, 800, 900]
            .iter()
            .enumerate()
        {
            let src = if i % 2 == 0 { "alpha" } else { "beta" };
            paths_by_age.push((*k, touch(dir.path(), src, *k, ".pcap", MIB)));
        }

        let report = run_sweep(dir.path(), &cfg(0, 5), now_at_minute(1000));

        assert_eq!(report.files_deleted, 4);
        assert_eq!(report.bytes_deleted, 4 * MIB);
        for (k, p) in &paths_by_age {
            if *k <= 400 {
                assert!(!p.exists(), "minute {k} should be deleted");
            } else {
                assert!(p.exists(), "minute {k} should be kept");
            }
        }
    }

    #[test]
    fn age_then_size_combined() {
        let dir = tempfile::tempdir().unwrap();
        // current = 10_000. max_age = 1h = 60 min → age cutoff at 9_940.
        // 4 files older than cutoff (9_900, 9_910, 9_920, 9_930) — all deleted by age.
        // 4 files within window (9_950, 9_960, 9_970, 9_980), each 2 MiB → 8 MiB.
        // Cap = 4 MiB → size sweep deletes the two oldest.
        let p_age_1 = touch(dir.path(), "s", 9_900, ".pcap", MIB);
        let p_age_2 = touch(dir.path(), "s", 9_910, ".pcap", MIB);
        let p_age_3 = touch(dir.path(), "s", 9_920, ".pcap", MIB);
        let p_age_4 = touch(dir.path(), "s", 9_930, ".pcap", MIB);
        let p_size_evict_1 = touch(dir.path(), "s", 9_950, ".pcap", 2 * MIB);
        let p_size_evict_2 = touch(dir.path(), "s", 9_960, ".pcap", 2 * MIB);
        let p_keep_1 = touch(dir.path(), "s", 9_970, ".pcap", 2 * MIB);
        let p_keep_2 = touch(dir.path(), "s", 9_980, ".pcap", 2 * MIB);

        let report = run_sweep(dir.path(), &cfg(1, 4), now_at_minute(10_000));

        assert_eq!(report.files_deleted, 6);
        assert_eq!(report.bytes_deleted, 4 * MIB + 4 * MIB);
        for p in [
            p_age_1,
            p_age_2,
            p_age_3,
            p_age_4,
            p_size_evict_1,
            p_size_evict_2,
        ] {
            assert!(!p.exists(), "{} should be deleted", p.display());
        }
        assert!(p_keep_1.exists());
        assert!(p_keep_2.exists());
    }

    #[test]
    fn foreign_files_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("s");
        fs::create_dir_all(&src_dir).unwrap();
        // A genuinely-named pcap that's old.
        let p_real = touch(dir.path(), "s", 100, ".pcap", MIB);
        // A foreign file in a source dir — must never be touched.
        let p_garbage = src_dir.join("garbage.txt");
        fs::write(&p_garbage, b"hello").unwrap();
        // A pcap that doesn't match minute_label format.
        let p_bad_pcap = src_dir.join("not-a-minute.pcap");
        fs::write(&p_bad_pcap, b"x").unwrap();
        // A subdir nested deeper than the layout.
        let p_nested = src_dir.join("sub");
        fs::create_dir_all(&p_nested).unwrap();
        let p_nested_file = p_nested.join("19700101T0000.pcap");
        fs::write(&p_nested_file, b"y").unwrap();

        // Run with aggressive settings.
        run_sweep(dir.path(), &cfg(1, 1), now_at_minute(10_000));

        assert!(!p_real.exists(), "real old file deleted by age");
        assert!(p_garbage.exists(), "non-pcap file untouched");
        assert!(p_bad_pcap.exists(), "pcap with bad name untouched");
        assert!(p_nested_file.exists(), "nested file untouched");
    }

    #[test]
    fn empty_rules_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let p = touch(dir.path(), "s", 100, ".pcap", MIB);

        let report = run_sweep(dir.path(), &cfg(0, 0), now_at_minute(10_000));
        assert!(report.is_noop());
        assert!(p.exists(), "no rules → nothing deleted");
    }

    #[test]
    fn missing_dump_dir_is_silent_noop() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("does-not-exist");
        let report = run_sweep(&missing, &cfg(1, 1), now_at_minute(10_000));
        assert_eq!(
            report,
            SweepReport::default(),
            "missing dump_dir must not surface as an error"
        );
    }

    #[test]
    fn snappy_files_are_recognized_and_deletable() {
        let dir = tempfile::tempdir().unwrap();
        let p = touch(dir.path(), "s", 100, ".pcap.snappy", MIB);
        let report = run_sweep(dir.path(), &cfg(1, 0), now_at_minute(10_000));
        assert_eq!(report.files_deleted, 1);
        assert!(!p.exists());
    }

    #[test]
    fn enoent_during_unlink_is_silent_noop_not_an_error() {
        // External `rm`, an admin cleanup script, or any other actor can
        // delete a file between our `read_dir` walk and the `remove_file`
        // call. ENOENT must NOT count as an error (we didn't fail, we
        // just lost a benign race) and must NOT produce a warn line.
        let dir = tempfile::tempdir().unwrap();
        let mut warner = ThrottledWarn::new(WARN_THROTTLE);
        let mut report = SweepReport::default();
        let ghost = dir.path().join("never-existed.pcap");

        let outcome = try_unlink(&ghost, &mut warner, &mut report);

        assert_eq!(outcome, UnlinkOutcome::AlreadyGone);
        assert_eq!(report.errors, 0, "ENOENT must not count as an error");
    }

    #[test]
    fn try_unlink_real_removal_returns_removed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("real.pcap");
        std::fs::write(&p, b"x").unwrap();
        let mut warner = ThrottledWarn::new(WARN_THROTTLE);
        let mut report = SweepReport::default();

        let outcome = try_unlink(&p, &mut warner, &mut report);

        assert_eq!(outcome, UnlinkOutcome::Removed);
        assert_eq!(report.errors, 0);
        assert!(!p.exists());
    }

    #[test]
    fn empty_source_dirs_are_kept_to_avoid_dumper_race() {
        // The dumper caches a SourceWriter on first packet and assumes its
        // dir exists for all future rotations. If retention pruned an idle
        // source's dir, the dumper would fail on next packet. Source dirs
        // therefore stay even when emptied — a few KiB per idle source
        // beats a recovery code path on the hot capture loop.
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("s");
        let p = touch(dir.path(), "s", 100, ".pcap", 0);
        let _ = run_sweep(dir.path(), &cfg(1, 0), now_at_minute(10_000));
        assert!(!p.exists(), "file deleted by age");
        assert!(
            src_dir.exists(),
            "source dir must NOT be pruned (dumper race)"
        );
    }
}
