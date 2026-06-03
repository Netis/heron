# h-storage-clickhouse

ClickHouse implementation of the `StorageBackend` trait — a drop-in alternative
to `h-storage-duckdb` for large-scale, high-throughput analytics deployments.

Talks to ClickHouse over its HTTP interface via the [`clickhouse`](https://crates.io/crates/clickhouse)
crate (async, serde RowBinary). Unlike the DuckDB backend there is no
`spawn_blocking`, no writer-mutex set, and no reader pool — the `Client` is a
cheap-to-clone HTTP connection pool.

## Schema

Module layout mirrors `h-storage-duckdb` 1:1 (`schema`, `rows`, `calls`,
`metrics`, `turns`, `sessions`, `exchanges`, `distincts`, `services`,
`retention`). Engines:

| Table | Engine | ORDER BY |
|---|---|---|
| `llm_calls`, `http_exchanges` | `MergeTree` | `(request_time, id)` |
| `llm_metrics` | `MergeTree` | `(granularity, timestamp, wire_api, model, server_ip)` |
| `llm_finish_metrics` | `MergeTree` | `(granularity, timestamp, finish_reason, …)` |
| `agent_turns` | `ReplacingMergeTree(_version)` | `turn_id` |

`agent_turns` is the only mutated table (`update_turn_metadata`): reads use
`FINAL`, and updates re-insert the full row with a higher `_version`. Timestamps
are `DateTime64(6, 'UTC')`; the crate maps them to `i64` microseconds (matching
the domain models — no chrono/time dependency).

## Configuration

```toml
[storage]
backend = "clickhouse"

[storage.clickhouse]
url               = "http://localhost:8123"
database          = "heron"   # created on startup if absent
user              = "default"
password          = ""
optimize_on_sweep = false     # OPTIMIZE TABLE ... FINAL after each retention sweep
```

Retention runs through the shared `[storage.retention]` schedule via lightweight
`DELETE` — identical to DuckDB.

## Testing

Pure-logic unit tests run under `cargo test --workspace` with no server.
Live-server integration tests are gated on `CLICKHOUSE_TEST_URL` and self-skip
when it is unset:

```bash
# Throwaway server (passwordless default user for local testing):
docker run -d --name ch-test -e CLICKHOUSE_SKIP_USER_SETUP=1 -p 8123:8123 \
  clickhouse/clickhouse-server:latest

CLICKHOUSE_TEST_URL=http://localhost:8123 cargo test -p h-storage-clickhouse

docker rm -f ch-test
```

Each test uses its own database (dropped + recreated), so the suite is
parallel-safe.

## Benchmark

`scripts/bench-storage.sh` (or `just bench-storage`) runs the identical workload
through both backends and prints a write-throughput + read-latency comparison.
Run it on the host where ClickHouse is on loopback so the engines compete on the
same hardware. See `server/app/heron/src/bin/storage_bench.rs`.
