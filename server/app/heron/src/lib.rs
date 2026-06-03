//! Library surface of the `heron` crate.
//!
//! Only the composition root is re-exported: everything else lives in the
//! per-stage `ts-*` crates. Exposing `Pipeline` lets integration tests under
//! `tests/` drive the full pipeline end-to-end without duplicating wiring.

pub mod pipeline;

pub use pipeline::{Pipeline, StageTask};

use std::sync::Arc;

use h_common::config::StorageConfig;
use h_common::error::{AppError, Result};
use h_storage::StorageBackend;
use h_storage_clickhouse::ClickHouseBackend;
use h_storage_duckdb::DuckDbBackend;

/// Dispatch on `config.backend` and instantiate the matching storage
/// backend. Lives in the assembly layer so adding `h-storage-postgres`
/// later is a one-arm extension here, not a fan-in to a backend-specific
/// crate.
pub fn create_backend(config: &StorageConfig) -> Result<Arc<dyn StorageBackend>> {
    match config.backend.as_str() {
        "duckdb" => Ok(Arc::new(DuckDbBackend::open(&config.duckdb.path)?)),
        "clickhouse" => Ok(Arc::new(ClickHouseBackend::new(&config.clickhouse)?)),
        other => Err(AppError::Config(format!(
            "unknown storage backend: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_backend_duckdb() {
        let mut config = StorageConfig::default();
        config.duckdb.path = ":memory:".to_string();
        assert!(create_backend(&config).is_ok());
    }

    #[test]
    fn test_create_backend_unknown() {
        let mut config = StorageConfig::default();
        config.backend = "postgres".to_string();
        assert!(create_backend(&config).is_err());
    }
}
