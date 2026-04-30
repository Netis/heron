pub mod backend;
pub mod buffer;
pub mod duckdb;
pub mod query;
pub mod retention;
pub mod sink;

use std::sync::Arc;

use ts_common::config::StorageConfig;
use ts_common::error::{AppError, Result};

pub use self::duckdb::DuckDbBackend;
pub use backend::StorageBackend;
pub use buffer::WriteBuffer;
pub use query::*;
pub use retention::{
    policy_from_config, spawn_retention_task, RetentionPolicy, RetentionReport,
    DEFAULT_METRICS_RETENTION_DAYS,
};
pub use sink::{spawn_storage_sink_stage, StorageSinkConfig};

/// Create a storage backend from configuration.
pub fn create_backend(config: &StorageConfig) -> Result<Arc<dyn StorageBackend>> {
    match config.backend.as_str() {
        "duckdb" => {
            let backend = DuckDbBackend::open(&config.duckdb.path)?;
            Ok(Arc::new(backend))
        }
        other => Err(AppError::Config(format!(
            "unknown storage backend: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts_common::config::StorageConfig;

    #[test]
    fn test_create_backend_duckdb() {
        let mut config = StorageConfig::default();
        config.duckdb.path = ":memory:".to_string();
        let backend = create_backend(&config);
        assert!(backend.is_ok());
    }

    #[test]
    fn test_create_backend_unknown() {
        let mut config = StorageConfig::default();
        config.backend = "postgres".to_string();
        let result = create_backend(&config);
        assert!(result.is_err());
    }
}
