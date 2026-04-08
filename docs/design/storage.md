# Storage Module Design

## Overview

The `ts-storage` crate provides a pluggable storage abstraction. The pipeline writes `LlmRequest`, `LlmLoop`, and `LlmMetric` through a unified trait; the API layer queries through the same trait. Backend-specific details are encapsulated in each implementation.

## StorageBackend Trait

```rust
pub trait StorageBackend: Send + Sync {
    // Write
    async fn write_requests(&self, requests: &[LlmRequest]) -> Result<()>;
    async fn write_metrics(&self, metrics: &[LlmMetric]) -> Result<()>;
    async fn insert_loop(&self, loop_record: &LlmLoop) -> Result<()>;
    async fn update_loop(&self, loop_record: &LlmLoop) -> Result<()>;

    // Query
    async fn query_requests(&self, query: &RequestQuery) -> Result<Vec<LlmRequest>>;
    async fn query_loops(&self, query: &LoopQuery) -> Result<Vec<LlmLoop>>;
    async fn query_metrics(&self, query: &MetricsQuery) -> Result<Vec<LlmMetric>>;
    async fn get_request(&self, id: &str) -> Result<Option<LlmRequest>>;
    async fn get_loop(&self, id: &str) -> Result<Option<LlmLoop>>;

    // Lifecycle
    async fn migrate(&self) -> Result<()>;
    async fn cleanup_expired(&self) -> Result<()>;
}
```

## Write Path

```
LlmRequest + LlmLoop ──▶ WriteBuffer ──▶ batch flush ──▶ StorageBackend
LlmMetric            ──▶ WriteBuffer ──▶ batch flush ──▶ StorageBackend
```

- `LlmRequest` and `LlmMetric` are batched (count + time thresholds)
- `LlmLoop`: `insert_loop()` on start, `update_loop()` on end — not batched

## Storage Backends

| Backend | Use case | Key crate |
|---------|----------|-----------|
| SQLite | Single-node, POC, edge | `sqlx` (sqlite feature) |
| PostgreSQL | Mid-scale production | `sqlx` (postgres feature) |
| ClickHouse | Large-scale, high-throughput analytics | `clickhouse-rs` |

Selected via configuration at startup. See [schema.md](schema.md) for backend adaptation notes.

## File Structure

```
ts-storage/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── traits.rs           # StorageBackend trait, query types
    ├── buffer.rs           # WriteBuffer (batch + timed flush)
    ├── sqlite.rs
    ├── postgres.rs
    └── clickhouse.rs
```
