# Storage Module Design

## Overview

The `ts-storage` crate provides pluggable storage abstraction. All writes are append-only and batched. The concrete trait interface will be shaped during implementation as we understand each backend's capability model.

## Core Principles

- **Pluggable backend**: DuckDB / PostgreSQL / ClickHouse selected via config at startup
- **Append-only writes**: all entities (LlmCall, LlmMetric) are INSERT-only, no UPDATE
- **Batch writes**: WriteBuffer collects records in memory, flushes on count or time threshold
- **Read/write separation**: write path (pipeline → buffer → DB) and read path (API → DB) may have different optimization strategies per backend
- **Backend-specific queries**: query interface may expose backend-specific capabilities rather than forcing a lowest-common-denominator abstraction

## Write Path

```
LlmCall ───┬──▶ WriteBuffer ──▶ batch flush ──▶ DB
LlmMetric ─┘
```

## Storage Backends

| Backend | Use case | Key crate |
|---------|----------|-----------|
| DuckDB | Default, single-node, dev, edge | `duckdb-rs` |
| PostgreSQL | Mid-scale production | `sqlx` (postgres feature) |
| ClickHouse | Large-scale, high-throughput analytics | `clickhouse-rs` |

See [schema.md](schema.md) for data schema and backend adaptation notes.

## File Structure

```
ts-storage/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── buffer.rs           # WriteBuffer (batch + timed flush)
    ├── retention.rs        # RetentionPolicy/Report + background sweeper
    ├── duckdb.rs
    ├── postgres.rs
    └── clickhouse.rs
```

## Retention

`StorageBackend::apply_retention(policy)` is a dialect-neutral trait method that each backend implements with its own DELETE / partition-drop / TTL strategy. A background task (`spawn_retention_task`) drives it on a fixed interval when enabled. Calls, turns, and metrics have independent TTLs; metrics are keyed per-granularity (`10s`/`1m`/`5m`/`1h`). See [schema.md § Data Lifecycle](schema.md#data-lifecycle) for the config shape.
