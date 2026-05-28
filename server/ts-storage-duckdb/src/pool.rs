//! Async-friendly connection pool over sync DuckDB read connections.
//!
//! `acquire()` is async (awaits a semaphore permit); the returned handle
//! dereferences to `&Connection` and is safe to move into `spawn_blocking`.
//!
//! The pool's contents are reachable via an `Arc<ReadPoolInner>`
//! indirection so that the **entire** working set can be swapped at
//! once via [`ReadPool::replace_all`]. This is what `reopen_all_connections`
//! relies on to recover from a DuckDB in-process-instance FATAL: a
//! `PooledConn` holds a private `Arc` snapshot to the inner taken at
//! `acquire` time, so when it drops it pushes the connection back into
//! its own (possibly already-orphaned) inner — not into the post-swap
//! current pool. The old inner is then dropped once the last in-flight
//! caller releases it, taking its stale connections with it. Net effect:
//! no stale connection ever re-enters circulation.
use std::sync::{Arc, Mutex as StdMutex};

use duckdb::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use ts_common::error::{AppError, Result};

struct ReadPoolInner {
    conns: StdMutex<Vec<Connection>>,
}

#[derive(Clone)]
pub(crate) struct ReadPool {
    inner: Arc<StdMutex<Arc<ReadPoolInner>>>,
    semaphore: Arc<Semaphore>,
    #[cfg(feature = "fault-injection")]
    fault_set: crate::fault_injection::FaultSet,
}

impl ReadPool {
    pub(crate) fn new(
        conns: Vec<Connection>,
        #[cfg(feature = "fault-injection")] fault_set: crate::fault_injection::FaultSet,
    ) -> Self {
        let size = conns.len();
        let inner = Arc::new(ReadPoolInner {
            conns: StdMutex::new(conns),
        });
        Self {
            inner: Arc::new(StdMutex::new(inner)),
            semaphore: Arc::new(Semaphore::new(size)),
            #[cfg(feature = "fault-injection")]
            fault_set,
        }
    }

    pub(crate) async fn acquire(&self) -> Result<PooledConn> {
        #[cfg(feature = "fault-injection")]
        {
            use crate::fault_injection::FaultPoint;
            if self.fault_set.should_fire(FaultPoint::ReadPoolPoisoned) {
                return Err(crate::fault_injection::read_pool_poisoned_error());
            }
        }
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| AppError::Storage(format!("read pool closed: {e}")))?;
        let inner_snapshot = {
            let guard = self
                .inner
                .lock()
                .map_err(|e| AppError::Storage(format!("read pool poisoned: {e}")))?;
            guard.clone()
        };
        let conn = {
            let mut guard = inner_snapshot
                .conns
                .lock()
                .map_err(|e| AppError::Storage(format!("read pool conns poisoned: {e}")))?;
            guard
                .pop()
                .ok_or_else(|| AppError::Storage("read pool invariant violated".to_string()))?
        };
        Ok(PooledConn {
            conn: Some(conn),
            inner: inner_snapshot,
            _permit: permit,
        })
    }

    /// Atomically replace the pool's connection set with `new_conns`.
    /// The caller is responsible for ensuring `new_conns.len()` equals
    /// the pool's original size (the semaphore's permit count is left
    /// unchanged; passing a different size would either over-issue
    /// permits or stall acquires).
    ///
    /// In-flight `PooledConn` handles taken before this call drop back
    /// into the previous inner, which then has no other Arc references
    /// and is freed along with its now-stale connections. Subsequent
    /// `acquire()` calls see the new inner.
    pub(crate) fn replace_all(&self, new_conns: Vec<Connection>) -> Result<()> {
        let new_inner = Arc::new(ReadPoolInner {
            conns: StdMutex::new(new_conns),
        });
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| AppError::Storage(format!("read pool poisoned: {e}")))?;
        *guard = new_inner;
        Ok(())
    }
}

pub(crate) struct PooledConn {
    conn: Option<Connection>,
    inner: Arc<ReadPoolInner>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            if let Ok(mut g) = self.inner.conns.lock() {
                g.push(c);
            }
        }
    }
}

impl std::ops::Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("conn present until drop")
    }
}
