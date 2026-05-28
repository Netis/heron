//! Deterministic fault injection for backend-recovery tests.
//!
//! Entirely feature-gated: when the `fault-injection` cargo feature is off,
//! this module does not compile and call sites guarded by
//! `#[cfg(feature = "fault-injection")]` evaporate. Production binaries
//! pay zero overhead.
//!
//! Why this exists: the canonical PR#48-class incident (DuckDB FATAL,
//! read pool stayed poisoned, prod 500-looped three times) is exactly
//! the kind of thing a unit test should pin down — but the FATAL itself
//! cannot be triggered deterministically from a synthetic load. Fault
//! injection lets the recovery path be exercised on demand and verified
//! end-to-end (write returns FATAL → reopen → every downstream surface
//! works) without waiting for prod traffic to manifest the bug again.
//!
//! Scope is per-backend: each `DuckDbBackend` owns its own armed-fault
//! set so parallel tests running with the `fault-injection` feature do
//! not leak state across one another.
//!
//! Usage in tests:
//!
//! ```ignore
//! let _guard = FaultGuard::arm(&backend, FaultPoint::DuckDbInvalidate);
//! let err = backend.write_turns(vec![mk_turn(0)]).await.unwrap_err();
//! // _guard's Drop disarms automatically — no leakage into sibling tests.
//! ```

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ts_common::error::AppError;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum FaultPoint {
    /// Make the next write path return a synthesized FATAL invalidation
    /// error, mimicking DuckDB's in-process instance becoming unusable.
    DuckDbInvalidate,
    /// Make the next read-pool acquire surface a poisoned-pool error.
    ReadPoolPoisoned,
    /// Make the next write path return a synthesized disk-full error.
    DiskFull,
}

/// Per-backend armed-fault set. Cloning an instance shares the same
/// inner storage — that's what lets the backend and its read pool see
/// the same armed-fault state without a global.
#[derive(Clone, Default)]
pub struct FaultSet {
    inner: Arc<Mutex<HashSet<FaultPoint>>>,
}

impl FaultSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn arm(&self, point: FaultPoint) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(point);
        }
    }

    pub fn disarm(&self, point: FaultPoint) {
        if let Ok(mut g) = self.inner.lock() {
            g.remove(&point);
        }
    }

    pub fn disarm_all(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.clear();
        }
    }

    pub fn should_fire(&self, point: FaultPoint) -> bool {
        self.inner
            .lock()
            .map(|g| g.contains(&point))
            .unwrap_or(false)
    }
}

/// RAII guard. Arms on construction, disarms on drop. Use in tests so
/// that a panic-after-arm or an early return never leaks the armed state.
pub struct FaultGuard {
    set: FaultSet,
    point: FaultPoint,
}

impl FaultGuard {
    pub fn arm(set: &FaultSet, point: FaultPoint) -> Self {
        set.arm(point);
        Self {
            set: set.clone(),
            point,
        }
    }
}

impl Drop for FaultGuard {
    fn drop(&mut self) {
        self.set.disarm(self.point);
    }
}

/// Synthesize the `AppError` that a real DuckDB FATAL invalidation
/// surfaces to callers. Used by instrumented write paths so injected
/// faults are indistinguishable from production failures from the
/// recovery code's perspective.
pub fn fatal_invalidate_error() -> AppError {
    AppError::Storage("duckdb FATAL: instance invalidated (fault-injection)".to_string())
}

/// Synthesize the `AppError` for a disk-full write failure.
pub fn disk_full_error() -> AppError {
    AppError::Storage("write failed: ENOSPC (fault-injection)".to_string())
}

/// Synthesize the `AppError` for a poisoned read pool.
pub fn read_pool_poisoned_error() -> AppError {
    AppError::Storage("read pool poisoned (fault-injection)".to_string())
}
