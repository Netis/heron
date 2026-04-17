# ts-storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the ts-storage crate with DuckDB backend and WriteBuffer for persisting LlmCall and LlmMetric records.

**Architecture:** A `StorageBackend` trait defines async batch-write operations. `DuckDbBackend` implements it using the `duckdb` crate with `spawn_blocking` for async bridging. Two independent `WriteBuffer` instances (one for calls, one for metrics) accumulate records from pipeline workers and flush on size or time thresholds.

**Tech Stack:** Rust, DuckDB (via `duckdb` crate with bundled feature), Tokio, async-trait

---

## File Structure

| File | Responsibility |
|------|---------------|
| `server/ts-storage/Cargo.toml` | Crate manifest |
| `server/ts-storage/src/lib.rs` | Re-exports, `create_backend()` factory |
| `server/ts-storage/src/backend.rs` | `StorageBackend` trait definition |
| `server/ts-storage/src/duckdb.rs` | `DuckDbBackend` — schema init + batch writes |
| `server/ts-storage/src/buffer.rs` | `WriteBuffer<T>` + `WriteBufferHandle<T>` |
| `server/ts-common/src/error.rs` | Add `Storage` variant to `AppError` |
| `server/ts-common/src/config.rs` | Add `batch_size` + `flush_interval_secs` to `StorageConfig` |
| `server/Cargo.toml` | Add `duckdb` + `ts-storage` to workspace dependencies |

---

### Task 1: Add Storage error variant and config fields to ts-common

**Files:**
- Modify: `server/ts-common/src/error.rs`
- Modify: `server/ts-common/src/config.rs`

- [ ] **Step 1: Add `Storage` variant to `AppError`**

In `server/ts-common/src/error.rs`, add the variant:

```rust
#[derive(Debug, Error)]
pub enum AppError {
    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(String),
}
```

- [ ] **Step 2: Add buffer config fields to `StorageConfig`**

In `server/ts-common/src/config.rs`, add `batch_size` and `flush_interval_secs` to `StorageConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,
    #[serde(default)]
    pub duckdb: DuckDbConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            batch_size: default_batch_size(),
            flush_interval_secs: default_flush_interval_secs(),
            duckdb: DuckDbConfig::default(),
        }
    }
}

fn default_batch_size() -> usize {
    1000
}

fn default_flush_interval_secs() -> u64 {
    5
}
```

- [ ] **Step 3: Update `default.toml`**

Add buffer config to `server/config/default.toml`:

```toml
[storage]
backend = "duckdb"
batch_size = 1000
flush_interval_secs = 5
```

- [ ] **Step 4: Verify compilation**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check -p ts-common`
Expected: compiles successfully

- [ ] **Step 5: Commit**

```bash
git add server/ts-common/src/error.rs server/ts-common/src/config.rs server/config/default.toml
git commit -m "feat(ts-common): add Storage error variant and buffer config fields"
```

---

### Task 2: Scaffold ts-storage crate with StorageBackend trait

**Files:**
- Create: `server/ts-storage/Cargo.toml`
- Create: `server/ts-storage/src/lib.rs`
- Create: `server/ts-storage/src/backend.rs`
- Modify: `server/Cargo.toml` (workspace deps)

- [ ] **Step 1: Add workspace dependencies**

In `server/Cargo.toml`, add to `[workspace.dependencies]`:

```toml
duckdb = { version = "1", features = ["bundled"] }
ts-storage = { path = "ts-storage" }
```

- [ ] **Step 2: Create `ts-storage/Cargo.toml`**

```toml
[package]
name = "ts-storage"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
ts-common.workspace = true
ts-llm.workspace = true
ts-metrics.workspace = true
duckdb.workspace = true
tokio.workspace = true
async-trait.workspace = true
tracing.workspace = true
```

- [ ] **Step 3: Create `backend.rs` with `StorageBackend` trait**

```rust
use async_trait::async_trait;
use ts_llm::model::LlmCall;
use ts_metrics::model::LlmMetric;

use ts_common::error::Result;

/// Pluggable storage backend for persisting LLM telemetry data.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Create tables/schemas if they don't exist.
    async fn init(&self) -> Result<()>;

    /// Batch-write LlmCall records.
    async fn write_calls(&self, calls: &[LlmCall]) -> Result<()>;

    /// Batch-write LlmMetric records.
    async fn write_metrics(&self, metrics: &[LlmMetric]) -> Result<()>;
}
```

- [ ] **Step 4: Create `lib.rs` with re-exports**

```rust
pub mod backend;

pub use backend::StorageBackend;
```

- [ ] **Step 5: Verify compilation**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check -p ts-storage`
Expected: compiles successfully

- [ ] **Step 6: Commit**

```bash
git add server/Cargo.toml server/ts-storage/
git commit -m "feat(ts-storage): scaffold crate with StorageBackend trait"
```

---

### Task 3: Implement DuckDbBackend — schema init

**Files:**
- Create: `server/ts-storage/src/duckdb.rs`
- Modify: `server/ts-storage/src/lib.rs`

- [ ] **Step 1: Write test for DuckDB schema initialization**

Add inline test in `server/ts-storage/src/duckdb.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageBackend;

    fn in_memory_backend() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
    }

    #[tokio::test]
    async fn test_init_creates_tables() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let conn = backend.conn.lock().unwrap();
        // Verify llm_calls table exists by querying it
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM llm_calls").unwrap();
        let count: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);

        // Verify llm_metrics table exists
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM llm_metrics").unwrap();
        let count: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_init_is_idempotent() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        backend.init().await.unwrap(); // second call should not error
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: compilation error — `DuckDbBackend` does not exist yet

- [ ] **Step 3: Implement `DuckDbBackend` with `init()`**

In `server/ts-storage/src/duckdb.rs`:

```rust
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use duckdb::Connection;
use tracing::info;
use ts_common::error::{AppError, Result};
use ts_llm::model::LlmCall;
use ts_metrics::model::LlmMetric;

use crate::StorageBackend;

/// DuckDB storage backend.
pub struct DuckDbBackend {
    conn: Arc<Mutex<Connection>>,
}

impl DuckDbBackend {
    /// Open a DuckDB database at the given path.
    /// Use ":memory:" for in-memory databases (testing).
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| AppError::Storage(format!("failed to open duckdb: {e}")))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

const CREATE_LLM_CALLS: &str = "
CREATE TABLE IF NOT EXISTS llm_calls (
    id                VARCHAR NOT NULL,
    tenant_id         VARCHAR,
    client_ip         VARCHAR NOT NULL,
    client_port       USMALLINT NOT NULL,
    server_ip         VARCHAR NOT NULL,
    server_port       USMALLINT NOT NULL,
    request_time      TIMESTAMP NOT NULL,
    response_time     TIMESTAMP,
    complete_time     TIMESTAMP,
    provider          VARCHAR NOT NULL,
    model             VARCHAR NOT NULL,
    api_type          VARCHAR NOT NULL,
    is_stream         BOOLEAN NOT NULL,
    request_path      VARCHAR NOT NULL,
    status_code       USMALLINT,
    finish_reason     VARCHAR,
    input_tokens      UINTEGER,
    output_tokens     UINTEGER,
    total_tokens      UINTEGER,
    ttfb_ms           DOUBLE,
    e2e_latency_ms    DOUBLE,
    request_body      VARCHAR,
    response_body     VARCHAR
);
";

const CREATE_LLM_METRICS: &str = "
CREATE TABLE IF NOT EXISTS llm_metrics (
    timestamp           TIMESTAMP NOT NULL,
    granularity         VARCHAR NOT NULL,
    provider            VARCHAR NOT NULL,
    model               VARCHAR NOT NULL,
    server_ip           VARCHAR NOT NULL,
    request_count       UBIGINT NOT NULL,
    stream_count        UBIGINT NOT NULL,
    non_stream_count    UBIGINT NOT NULL,
    concurrency_avg     DOUBLE NOT NULL,
    concurrency_max     UINTEGER NOT NULL,
    total_input_tokens  UBIGINT NOT NULL,
    total_output_tokens UBIGINT NOT NULL,
    input_tokens_avg    DOUBLE,
    input_tokens_p50    DOUBLE,
    input_tokens_p95    DOUBLE,
    input_tokens_p99    DOUBLE,
    error_count         UBIGINT NOT NULL,
    error_4xx_count     UBIGINT NOT NULL,
    error_429_count     UBIGINT NOT NULL,
    error_5xx_count     UBIGINT NOT NULL,
    ttfb_avg            DOUBLE,
    ttfb_p50            DOUBLE,
    ttfb_p95            DOUBLE,
    ttfb_p99            DOUBLE,
    e2e_avg             DOUBLE,
    e2e_p50             DOUBLE,
    e2e_p95             DOUBLE,
    e2e_p99             DOUBLE
);
";

#[async_trait]
impl StorageBackend for DuckDbBackend {
    async fn init(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|e| {
                AppError::Storage(format!("failed to lock connection: {e}"))
            })?;
            conn.execute_batch(CREATE_LLM_CALLS)
                .map_err(|e| AppError::Storage(format!("failed to create llm_calls: {e}")))?;
            conn.execute_batch(CREATE_LLM_METRICS)
                .map_err(|e| AppError::Storage(format!("failed to create llm_metrics: {e}")))?;
            info!("storage tables initialized");
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn write_calls(&self, _calls: &[LlmCall]) -> Result<()> {
        todo!() // Implemented in Task 4
    }

    async fn write_metrics(&self, _metrics: &[LlmMetric]) -> Result<()> {
        todo!() // Implemented in Task 5
    }
}
```

- [ ] **Step 4: Add `duckdb` module to `lib.rs`**

Update `server/ts-storage/src/lib.rs`:

```rust
pub mod backend;
pub mod duckdb;

pub use backend::StorageBackend;
pub use self::duckdb::DuckDbBackend;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: 2 tests pass

- [ ] **Step 6: Commit**

```bash
git add server/ts-storage/src/duckdb.rs server/ts-storage/src/lib.rs
git commit -m "feat(ts-storage): implement DuckDbBackend schema init"
```

---

### Task 4: Implement DuckDbBackend — write_calls

**Files:**
- Modify: `server/ts-storage/src/duckdb.rs`

- [ ] **Step 1: Write test for `write_calls`**

Add to the `tests` module in `server/ts-storage/src/duckdb.rs`:

```rust
    use std::net::IpAddr;
    use ts_llm::model::{ApiType, FinishReason, ProviderFormat};

    fn sample_call() -> LlmCall {
        LlmCall {
            id: "01912345-6789-7abc-def0-123456789abc".to_string(),
            provider: ProviderFormat::OpenAI,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            tenant_id: Some("tenant-abc".to_string()),
            request_time: 1_700_000_000_000_000, // microseconds
            response_time: Some(1_700_000_000_500_000),
            complete_time: Some(1_700_000_001_000_000),
            request_path: "/v1/chat/completions".to_string(),
            is_stream: true,
            request_body: Some(r#"{"model":"gpt-4"}"#.to_string()),
            status_code: Some(200),
            finish_reason: Some(FinishReason::Complete),
            response_body: Some(r#"{"choices":[...]}"#.to_string()),
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            ttfb_ms: Some(500.0),
            e2e_latency_ms: Some(1000.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 54321,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 8080,
        }
    }

    #[tokio::test]
    async fn test_write_calls_single() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(&[call]).await.unwrap();

        let conn = backend.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, model, is_stream, input_tokens FROM llm_calls").unwrap();
        let row = stmt.query_row([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Option<u32>>(3)?,
            ))
        }).unwrap();
        assert_eq!(row.0, "01912345-6789-7abc-def0-123456789abc");
        assert_eq!(row.1, "gpt-4");
        assert!(row.2);
        assert_eq!(row.3, Some(100));
    }

    #[tokio::test]
    async fn test_write_calls_empty_batch() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        backend.write_calls(&[]).await.unwrap(); // should not error
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: `test_write_calls_single` panics with `todo!()`

- [ ] **Step 3: Implement `write_calls`**

Replace the `todo!()` in `write_calls` with the Appender-based implementation in `server/ts-storage/src/duckdb.rs`:

```rust
    async fn write_calls(&self, calls: &[LlmCall]) -> Result<()> {
        if calls.is_empty() {
            return Ok(());
        }
        let conn = self.conn.clone();
        let calls = calls.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|e| {
                AppError::Storage(format!("failed to lock connection: {e}"))
            })?;
            let mut appender = conn.appender("llm_calls").map_err(|e| {
                AppError::Storage(format!("failed to create appender: {e}"))
            })?;
            for call in &calls {
                appender
                    .append_row(duckdb::params![
                        call.id,
                        call.tenant_id,
                        call.client_ip.to_string(),
                        call.client_port,
                        call.server_ip.to_string(),
                        call.server_port,
                        us_to_timestamp(call.request_time),
                        call.response_time.map(us_to_timestamp),
                        call.complete_time.map(us_to_timestamp),
                        call.provider.to_string(),
                        call.model,
                        call.api_type.to_string(),
                        call.is_stream,
                        call.request_path,
                        call.status_code,
                        call.finish_reason.map(|r| r.to_string()),
                        call.input_tokens,
                        call.output_tokens,
                        call.total_tokens,
                        call.ttfb_ms,
                        call.e2e_latency_ms,
                        call.request_body,
                        call.response_body,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append call: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush calls: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
```

Also add the timestamp helper function above the `impl` block:

```rust
/// Convert microseconds since epoch to a string DuckDB can parse as TIMESTAMP.
fn us_to_timestamp(us: i64) -> String {
    let secs = us / 1_000_000;
    let micros = (us % 1_000_000) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, micros * 1000)
        .unwrap_or_default();
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}
```

Add `chrono` to `server/ts-storage/Cargo.toml` dependencies:

```toml
chrono = "0.4"
```

And add `chrono` to `server/Cargo.toml` workspace dependencies:

```toml
chrono = "0.4"
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add server/Cargo.toml server/ts-storage/
git commit -m "feat(ts-storage): implement DuckDbBackend write_calls with Appender"
```

---

### Task 5: Implement DuckDbBackend — write_metrics

**Files:**
- Modify: `server/ts-storage/src/duckdb.rs`

- [ ] **Step 1: Write test for `write_metrics`**

Add to the `tests` module in `server/ts-storage/src/duckdb.rs`:

```rust
    fn sample_metric() -> LlmMetric {
        LlmMetric {
            timestamp_us: 1_700_000_000_000_000,
            granularity: "1m",
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            server_ip: "10.0.0.2".to_string(),
            request_count: 42,
            stream_count: 30,
            non_stream_count: 12,
            concurrency_avg: 3.5,
            concurrency_max: 8,
            total_input_tokens: 10000,
            total_output_tokens: 5000,
            input_tokens_avg: Some(238.1),
            input_tokens_p50: Some(200.0),
            input_tokens_p95: Some(500.0),
            input_tokens_p99: Some(800.0),
            error_count: 2,
            error_4xx_count: 1,
            error_429_count: 0,
            error_5xx_count: 1,
            ttfb_avg: Some(150.0),
            ttfb_p50: Some(120.0),
            ttfb_p95: Some(350.0),
            ttfb_p99: Some(500.0),
            e2e_avg: Some(1200.0),
            e2e_p50: Some(1000.0),
            e2e_p95: Some(2500.0),
            e2e_p99: Some(4000.0),
        }
    }

    #[tokio::test]
    async fn test_write_metrics_single() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let metric = sample_metric();
        backend.write_metrics(&[metric]).await.unwrap();

        let conn = backend.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT granularity, model, request_count, ttfb_p50 FROM llm_metrics")
            .unwrap();
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<f64>>(3)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, "1m");
        assert_eq!(row.1, "gpt-4");
        assert_eq!(row.2, 42);
        assert_eq!(row.3, Some(120.0));
    }

    #[tokio::test]
    async fn test_write_metrics_empty_batch() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        backend.write_metrics(&[]).await.unwrap();
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: `test_write_metrics_single` panics with `todo!()`

- [ ] **Step 3: Implement `write_metrics`**

Replace the `todo!()` in `write_metrics`:

```rust
    async fn write_metrics(&self, metrics: &[LlmMetric]) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        let conn = self.conn.clone();
        let metrics = metrics.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|e| {
                AppError::Storage(format!("failed to lock connection: {e}"))
            })?;
            let mut appender = conn.appender("llm_metrics").map_err(|e| {
                AppError::Storage(format!("failed to create appender: {e}"))
            })?;
            for m in &metrics {
                appender
                    .append_row(duckdb::params![
                        us_to_timestamp(m.timestamp_us),
                        m.granularity,
                        m.provider,
                        m.model,
                        m.server_ip,
                        m.request_count,
                        m.stream_count,
                        m.non_stream_count,
                        m.concurrency_avg,
                        m.concurrency_max,
                        m.total_input_tokens,
                        m.total_output_tokens,
                        m.input_tokens_avg,
                        m.input_tokens_p50,
                        m.input_tokens_p95,
                        m.input_tokens_p99,
                        m.error_count,
                        m.error_4xx_count,
                        m.error_429_count,
                        m.error_5xx_count,
                        m.ttfb_avg,
                        m.ttfb_p50,
                        m.ttfb_p95,
                        m.ttfb_p99,
                        m.e2e_avg,
                        m.e2e_p50,
                        m.e2e_p95,
                        m.e2e_p99,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append metric: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush metrics: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add server/ts-storage/src/duckdb.rs
git commit -m "feat(ts-storage): implement DuckDbBackend write_metrics with Appender"
```

---

### Task 6: Implement WriteBuffer and WriteBufferHandle

**Files:**
- Create: `server/ts-storage/src/buffer.rs`
- Modify: `server/ts-storage/src/lib.rs`

- [ ] **Step 1: Write tests for WriteBuffer**

Create `server/ts-storage/src/buffer.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn test_flush_on_batch_size() {
        let flush_count = Arc::new(AtomicUsize::new(0));
        let flush_count_clone = flush_count.clone();

        let (handle, buffer) = create_buffer::<i32>(
            3,                           // batch_size
            Duration::from_secs(60),     // long interval — won't trigger
            16,                          // channel capacity
        );

        let task = tokio::spawn(async move {
            buffer
                .run(move |batch| {
                    let fc = flush_count_clone.clone();
                    async move {
                        fc.fetch_add(batch.len(), Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        // Send 3 items — should trigger one flush
        for i in 0..3 {
            handle.send(i).await.unwrap();
        }
        // Send 3 more — should trigger another flush
        for i in 3..6 {
            handle.send(i).await.unwrap();
        }

        // Drop handle to signal shutdown
        drop(handle);
        task.await.unwrap();

        assert_eq!(flush_count.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn test_flush_on_interval() {
        let flush_count = Arc::new(AtomicUsize::new(0));
        let flush_count_clone = flush_count.clone();

        let (handle, buffer) = create_buffer::<i32>(
            1000,                          // large batch — won't trigger by size
            Duration::from_millis(50),     // short interval
            16,
        );

        let task = tokio::spawn(async move {
            buffer
                .run(move |batch| {
                    let fc = flush_count_clone.clone();
                    async move {
                        fc.fetch_add(batch.len(), Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        handle.send(1).await.unwrap();
        handle.send(2).await.unwrap();

        // Wait for time-based flush
        tokio::time::sleep(Duration::from_millis(150)).await;

        drop(handle);
        task.await.unwrap();

        assert_eq!(flush_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_flush_remaining_on_shutdown() {
        let flush_count = Arc::new(AtomicUsize::new(0));
        let flush_count_clone = flush_count.clone();

        let (handle, buffer) = create_buffer::<i32>(
            100,                          // won't trigger by size
            Duration::from_secs(60),      // won't trigger by time
            16,
        );

        let task = tokio::spawn(async move {
            buffer
                .run(move |batch| {
                    let fc = flush_count_clone.clone();
                    async move {
                        fc.fetch_add(batch.len(), Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
        });

        handle.send(1).await.unwrap();
        handle.send(2).await.unwrap();

        drop(handle);
        task.await.unwrap();

        // Should have flushed remaining 2 items on shutdown
        assert_eq!(flush_count.load(Ordering::SeqCst), 2);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: compilation error — `create_buffer` etc. don't exist

- [ ] **Step 3: Implement WriteBuffer and WriteBufferHandle**

Add the implementation above the tests in `server/ts-storage/src/buffer.rs`:

```rust
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, error};

use ts_common::error::Result;

/// Producer handle for sending records into a WriteBuffer.
/// Clone-friendly — hand one to each pipeline worker.
pub struct WriteBufferHandle<T> {
    tx: mpsc::Sender<T>,
}

impl<T> Clone for WriteBufferHandle<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<T: Send + 'static> WriteBufferHandle<T> {
    pub async fn send(&self, item: T) -> Result<()> {
        self.tx.send(item).await.map_err(|_| {
            ts_common::error::AppError::Storage("write buffer channel closed".to_string())
        })
    }
}

/// Consumer side — batches incoming records and flushes on size/time thresholds.
pub struct WriteBuffer<T> {
    rx: mpsc::Receiver<T>,
    batch_size: usize,
    flush_interval: Duration,
}

/// Create a paired (handle, buffer).
pub fn create_buffer<T: Send + 'static>(
    batch_size: usize,
    flush_interval: Duration,
    channel_capacity: usize,
) -> (WriteBufferHandle<T>, WriteBuffer<T>) {
    let (tx, rx) = mpsc::channel(channel_capacity);
    (
        WriteBufferHandle { tx },
        WriteBuffer {
            rx,
            batch_size,
            flush_interval,
        },
    )
}

impl<T: Send + 'static> WriteBuffer<T> {
    /// Run the buffer loop. Calls `flush_fn` with each batch.
    /// Returns when all senders are dropped and remaining items are flushed.
    pub async fn run<F, Fut>(mut self, flush_fn: F)
    where
        F: Fn(Vec<T>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut batch = Vec::with_capacity(self.batch_size);
        let mut interval = tokio::time::interval(self.flush_interval);
        // The first tick completes immediately — consume it so the first
        // real deadline is one full interval from now.
        interval.tick().await;

        loop {
            tokio::select! {
                item = self.rx.recv() => {
                    match item {
                        Some(item) => {
                            batch.push(item);
                            if batch.len() >= self.batch_size {
                                let to_flush = std::mem::replace(
                                    &mut batch,
                                    Vec::with_capacity(self.batch_size),
                                );
                                debug!(count = to_flush.len(), "flushing batch (size threshold)");
                                if let Err(e) = flush_fn(to_flush).await {
                                    error!("flush error: {e}");
                                }
                                interval.reset();
                            }
                        }
                        None => {
                            // Channel closed — flush remaining and exit
                            if !batch.is_empty() {
                                debug!(count = batch.len(), "flushing remaining batch (shutdown)");
                                if let Err(e) = flush_fn(batch).await {
                                    error!("flush error on shutdown: {e}");
                                }
                            }
                            return;
                        }
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        let to_flush = std::mem::replace(
                            &mut batch,
                            Vec::with_capacity(self.batch_size),
                        );
                        debug!(count = to_flush.len(), "flushing batch (time threshold)");
                        if let Err(e) = flush_fn(to_flush).await {
                            error!("flush error: {e}");
                        }
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 4: Add `buffer` module to `lib.rs`**

Update `server/ts-storage/src/lib.rs`:

```rust
pub mod backend;
pub mod buffer;
pub mod duckdb;

pub use backend::StorageBackend;
pub use buffer::{WriteBuffer, WriteBufferHandle, create_buffer};
pub use self::duckdb::DuckDbBackend;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: all tests pass (including buffer tests)

- [ ] **Step 6: Commit**

```bash
git add server/ts-storage/src/buffer.rs server/ts-storage/src/lib.rs
git commit -m "feat(ts-storage): implement WriteBuffer with size+time flush"
```

---

### Task 7: Add `create_backend` factory and finalize lib.rs

**Files:**
- Modify: `server/ts-storage/src/lib.rs`

- [ ] **Step 1: Write test for `create_backend`**

Add inline test to `server/ts-storage/src/lib.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: compilation error — `create_backend` doesn't exist

- [ ] **Step 3: Implement `create_backend`**

Update `server/ts-storage/src/lib.rs`:

```rust
pub mod backend;
pub mod buffer;
pub mod duckdb;

use std::sync::Arc;

use ts_common::config::StorageConfig;
use ts_common::error::{AppError, Result};

pub use backend::StorageBackend;
pub use buffer::{WriteBuffer, WriteBufferHandle, create_buffer};
pub use self::duckdb::DuckDbBackend;

/// Create a storage backend from configuration.
pub fn create_backend(config: &StorageConfig) -> Result<Arc<dyn StorageBackend>> {
    match config.backend.as_str() {
        "duckdb" => {
            let backend = DuckDbBackend::open(&config.duckdb.path)?;
            Ok(Arc::new(backend))
        }
        other => Err(AppError::Config(format!("unknown storage backend: {other}"))),
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
```

- [ ] **Step 4: Run all tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add server/ts-storage/src/lib.rs
git commit -m "feat(ts-storage): add create_backend factory function"
```

---

### Task 8: Final workspace check — ensure full build and all tests pass

**Files:** (none new)

- [ ] **Step 1: Run full workspace check**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check`
Expected: no errors across entire workspace

- [ ] **Step 2: Run all workspace tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test`
Expected: all tests pass across all crates

- [ ] **Step 3: Commit if any fixups were needed**

Only if changes were made to fix issues found in steps 1-2.
