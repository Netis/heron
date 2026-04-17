# ts-storage Design Spec

## Scope

Implement the `ts-storage` crate — pluggable storage layer for TokenScope. This iteration supports **DuckDB only** and writes **`llm_calls` + `llm_metrics`** (no `llm_traces`).

## Components

### 1. `StorageBackend` Trait (`backend.rs`)

```rust
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Create tables if they don't exist.
    async fn init(&self) -> Result<()>;
    /// Batch-write LlmCall records.
    async fn write_calls(&self, calls: &[LlmCall]) -> Result<()>;
    /// Batch-write LlmMetric records.
    async fn write_metrics(&self, metrics: &[LlmMetric]) -> Result<()>;
}
```

Minimal surface — query methods deferred to `ts-api` implementation.

### 2. DuckDB Backend (`duckdb.rs`)

- **Crate:** `duckdb` (C-based, blocking API).
- **Async bridging:** Wrap all DB calls in `tokio::task::spawn_blocking`.
- **Connection:** `Arc<Mutex<duckdb::Connection>>`. Single-writer is sufficient; DuckDB is single-process anyway.
- **Schema:** `init()` creates `llm_calls` and `llm_metrics` tables matching `docs/design/schema.md`. Column types per the "Storage Backend Adaptation Notes" table (VARCHAR for UUIDs, TIMESTAMP for times, etc.).
- **Batch insert:** Use DuckDB's `Appender` API for bulk inserts. Each `write_calls`/`write_metrics` call creates an appender, appends all rows, and flushes.
- **Timestamps:** `LlmCall` and `LlmMetric` store microseconds (i64). Convert to DuckDB TIMESTAMP via `/ 1_000_000` for seconds + `% 1_000_000` for microsecond fraction.

#### `llm_calls` Table Schema (DuckDB)

| Column | Type |
|--------|------|
| id | VARCHAR |
| tenant_id | VARCHAR |
| client_ip | VARCHAR |
| client_port | USMALLINT |
| server_ip | VARCHAR |
| server_port | USMALLINT |
| request_time | TIMESTAMP |
| response_time | TIMESTAMP |
| complete_time | TIMESTAMP |
| provider | VARCHAR |
| model | VARCHAR |
| api_type | VARCHAR |
| is_stream | BOOLEAN |
| request_path | VARCHAR |
| status_code | USMALLINT |
| finish_reason | VARCHAR |
| input_tokens | UINTEGER |
| output_tokens | UINTEGER |
| total_tokens | UINTEGER |
| ttfb_ms | DOUBLE |
| e2e_latency_ms | DOUBLE |
| request_body | VARCHAR |
| response_body | VARCHAR |

#### `llm_metrics` Table Schema (DuckDB)

| Column | Type |
|--------|------|
| timestamp | TIMESTAMP |
| granularity | VARCHAR |
| provider | VARCHAR |
| model | VARCHAR |
| server_ip | VARCHAR |
| request_count | UBIGINT |
| stream_count | UBIGINT |
| non_stream_count | UBIGINT |
| concurrency_avg | DOUBLE |
| concurrency_max | UINTEGER |
| total_input_tokens | UBIGINT |
| total_output_tokens | UBIGINT |
| input_tokens_avg | DOUBLE |
| input_tokens_p50 | DOUBLE |
| input_tokens_p95 | DOUBLE |
| input_tokens_p99 | DOUBLE |
| error_count | UBIGINT |
| error_4xx_count | UBIGINT |
| error_429_count | UBIGINT |
| error_5xx_count | UBIGINT |
| ttfb_avg | DOUBLE |
| ttfb_p50 | DOUBLE |
| ttfb_p95 | DOUBLE |
| ttfb_p99 | DOUBLE |
| e2e_avg | DOUBLE |
| e2e_p50 | DOUBLE |
| e2e_p95 | DOUBLE |
| e2e_p99 | DOUBLE |

### 3. WriteBuffer (`buffer.rs`)

Two independent instances: one for `LlmCall`, one for `LlmMetric`.

**`WriteBufferHandle<T>`** — producer side, holds `mpsc::Sender<T>`. Cloneable, handed to pipeline workers / metrics aggregator.

```rust
pub struct WriteBufferHandle<T> {
    tx: mpsc::Sender<T>,
}

impl<T> WriteBufferHandle<T> {
    pub async fn send(&self, item: T) -> Result<()>;
}
```

**`WriteBuffer`** — consumer side, runs as a Tokio task.

```rust
pub struct WriteBuffer<T> {
    rx: mpsc::Receiver<T>,
    batch: Vec<T>,
    batch_size: usize,
    flush_interval: Duration,
}
```

Flush strategy (size + time hybrid):
1. Receive items from channel, push into `Vec`.
2. Flush when `batch.len() >= batch_size` OR `flush_interval` elapsed since last flush.
3. Implementation uses `tokio::select!` on `rx.recv()` vs `tokio::time::sleep(remaining)`.
4. On channel close (all senders dropped), flush remaining items and exit.

The `WriteBuffer::run()` method takes an `Arc<dyn StorageBackend>` and a flush function selector (calls vs metrics). Use a closure/callback pattern to select which write method to call.

### 4. Factory Function (`lib.rs`)

```rust
pub fn create_backend(config: &StorageConfig) -> Result<Arc<dyn StorageBackend>> {
    match config.backend.as_str() {
        "duckdb" => Ok(Arc::new(DuckDbBackend::new(&config.duckdb)?)),
        other => Err(AppError::Config(format!("unknown storage backend: {other}"))),
    }
}
```

### 5. Configuration

Add `batch_size` and `flush_interval_secs` to `StorageConfig` in `ts-common`:

```rust
pub struct StorageConfig {
    pub backend: String,
    pub batch_size: usize,          // default 1000
    pub flush_interval_secs: u64,   // default 5
    pub duckdb: DuckDbConfig,
}
```

### 6. Error Handling

Add `Storage` variant to `AppError` in `ts-common`:

```rust
#[error("storage error: {0}")]
Storage(String),
```

## Module Structure

```
ts-storage/
├── Cargo.toml
└── src/
    ├── lib.rs          # re-exports, create_backend()
    ├── backend.rs      # StorageBackend trait
    ├── duckdb.rs       # DuckDbBackend
    └── buffer.rs       # WriteBuffer + WriteBufferHandle
```

## Dependencies

```toml
[dependencies]
ts-common = { workspace = true }
ts-llm = { workspace = true }
ts-metrics = { workspace = true }
duckdb = { version = "1", features = ["bundled"] }
tokio = { workspace = true }
async-trait = { workspace = true }
tracing = { workspace = true }
```

Add `duckdb` and `ts-storage` to workspace dependencies in root `Cargo.toml`.

## Not In Scope

- `llm_traces` table (experimental, deferred)
- Query/read methods (deferred to `ts-api` work)
- PostgreSQL / ClickHouse backends
- Data retention / cleanup
- Integration into the main pipeline binary (separate task)
