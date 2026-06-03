//! `Client` construction + thin statement-execution helpers. Replaces the
//! DuckDB backend's `pool.rs`: the `clickhouse` crate's `Client` is a
//! cheap-to-clone HTTP connection pool, so there is no writer-mutex set and no
//! reader pool to manage.

use clickhouse::Client;
use h_common::error::{AppError, Result};

use crate::ClickHouseBackend;

/// Build a `Client` from raw connection params. `database = None` yields a
/// server-scoped ("admin") client usable before the target database exists
/// (e.g. for `CREATE DATABASE`); `Some(db)` binds the default database.
pub(crate) fn build_client(
    url: &str,
    user: &str,
    password: &str,
    database: Option<&str>,
) -> Client {
    let mut c = Client::default()
        .with_url(url)
        .with_user(user)
        .with_password(password);
    if let Some(db) = database {
        c = c.with_database(db);
    }
    c
}

impl ClickHouseBackend {
    /// Database-unscoped client for bootstrap DDL (`CREATE DATABASE`). A
    /// `?database=heron` connection errors `UNKNOWN_DATABASE` until the
    /// database exists, so the create-database step must run here.
    pub(crate) fn admin_client(&self) -> Client {
        build_client(&self.url, &self.user, &self.password, None)
    }

    /// Run a statement that returns no rows (DDL, `INSERT ... SELECT`,
    /// `DELETE`, `OPTIMIZE`) on the database-scoped client.
    pub(crate) async fn exec(&self, sql: &str) -> Result<()> {
        self.client
            .query(sql)
            .execute()
            .await
            .map_err(|e| AppError::Storage(format!("clickhouse exec failed: {e}")))
    }
}

/// Batch-insert concrete `#[derive(Row)]` `rows` into `table` via the RowBinary
/// insert stream. A `macro_rules!` rather than a generic fn: the `Insert::write`
/// bound (`for<'a> Row::Value<'a>: Serialize`) is trivially satisfied for each
/// concrete owned row type (`Value = Self`) but awkward to spell generically.
/// Expands to statements whose `?` propagates to the enclosing `async fn`;
/// empty input is a no-op. The shared `WriteBuffer` already batches ~1000 rows
/// per call, matching the DuckDB appender's per-flush granularity.
macro_rules! insert_all {
    ($client:expr, $table:literal, $ty:ty, $rows:expr) => {{
        let rows: Vec<$ty> = $rows;
        if !rows.is_empty() {
            let mut insert = $client
                .insert::<$ty>($table)
                .await
                .map_err(|e| $crate::client::ch_err(concat!("open insert ", $table), e))?;
            for r in &rows {
                insert
                    .write(r)
                    .await
                    .map_err(|e| $crate::client::ch_err(concat!("write ", $table), e))?;
            }
            insert
                .end()
                .await
                .map_err(|e| $crate::client::ch_err(concat!("flush ", $table), e))?;
        }
    }};
}
pub(crate) use insert_all;

/// Map a `clickhouse::error::Error` into the app's storage error with `context`
/// describing the operation that failed.
pub(crate) fn ch_err(context: &str, e: clickhouse::error::Error) -> AppError {
    AppError::Storage(format!("clickhouse {context}: {e}"))
}
