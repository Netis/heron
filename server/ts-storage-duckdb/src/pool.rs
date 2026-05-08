//! Async-friendly connection pool over sync DuckDB read connections.
//!
//! `acquire()` is async (awaits a semaphore permit); the returned handle
//! dereferences to `&Connection` and is safe to move into `spawn_blocking`.
use std::sync::{Arc, Mutex as StdMutex};

use duckdb::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use ts_common::error::{AppError, Result};

#[derive(Clone)]
pub(crate) struct ReadPool {
    conns: Arc<StdMutex<Vec<Connection>>>,
    semaphore: Arc<Semaphore>,
}

impl ReadPool {
    pub(crate) fn new(conns: Vec<Connection>) -> Self {
        let size = conns.len();
        Self {
            conns: Arc::new(StdMutex::new(conns)),
            semaphore: Arc::new(Semaphore::new(size)),
        }
    }

    pub(crate) async fn acquire(&self) -> Result<PooledConn> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| AppError::Storage(format!("read pool closed: {e}")))?;
        let conn = {
            let mut guard = self
                .conns
                .lock()
                .map_err(|e| AppError::Storage(format!("read pool poisoned: {e}")))?;
            guard
                .pop()
                .ok_or_else(|| AppError::Storage("read pool invariant violated".to_string()))?
        };
        Ok(PooledConn {
            conn: Some(conn),
            pool: self.conns.clone(),
            _permit: permit,
        })
    }
}

pub(crate) struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<StdMutex<Vec<Connection>>>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            if let Ok(mut g) = self.pool.lock() {
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
