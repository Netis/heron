# Storage benchmark: ClickHouse vs DuckDB

Reproducible via `just bench-storage` (`scripts/bench-storage.sh`), which drives
the identical synthetic workload through both backends via the `StorageBackend`
trait (`server/app/heron/src/bin/storage_bench.rs`).

## Methodology

- **Same host.** DuckDB (embedded) and ClickHouse (`clickhouse/clickhouse-server`
  in Docker, on **loopback**) run on one machine so the engines compete on the
  same CPU/disk with no network in the path. Both binaries built `--release`.
- Workload: synthetic `llm_calls` (~2 KB request+response body each, with a real
  `usage` block), `llm_metrics` (both `('*','*','*')` rollup and `(W,M,'*')`
  tiers, as the live aggregator emits), and `agent_turns`. Writes go through the
  same batched path the pipeline sink uses; reads are the dashboard's hot
  queries, each timed over 30 iterations (p50 reported).
- **Batch size is the dominant write variable for ClickHouse** and is reported
  explicitly. The pipeline sink defaults to `batch_size = 1000`.

> A remote run (DuckDB on the dev box, ClickHouse on a separate server over the
> LAN) was also taken; it is **not** reported as an engine comparison because
> per-query / per-batch network round-trips dominate it (CH query p50 rose from
> ~8 ms loopback to ~40 ms over the LAN). The takeaway from it stands on its
> own: a remote ClickHouse pays a network hop per query that an embedded DuckDB
> does not — budget for it in latency-sensitive read paths.

## Results — small batches (`batch = 1000`, the sink default)

100k calls / 50k metrics / 20k turns.

| write rows/sec | DuckDB | ClickHouse | CH / Duck |
|---|--:|--:|--:|
| calls   | 26,800  | 14,600 | 0.54× |
| metrics | 293,000 | 15,100 | 0.05× |
| turns   | 290,000 | 14,500 | 0.05× |

| query p50 (ms) | DuckDB | ClickHouse | CH / Duck |
|---|--:|--:|--:|
| calls       | 5.6  | 9.0   | 1.6× |
| summary     | 0.8  | 5.7   | 7.3× |
| timeseries  | 2.6  | 10.8  | 4.2× |
| turns       | 2.8  | 7.8   | 2.8× |
| services    | 18.0 | 174.9 | 9.7× |

At 1000-row batches ClickHouse is **part-creation-bound**: each small INSERT
materialises a new MergeTree part, so throughput is ~15k rows/s regardless of
row width — slower than DuckDB's appender. This is the well-known ClickHouse
small-insert anti-pattern.

## Results — large batches (`batch = 50000`)

400k calls / 400k metrics / 200k turns.

| write rows/sec | DuckDB | ClickHouse | CH / Duck |
|---|--:|--:|--:|
| calls   | 97,000  | 279,000 | **2.88×** |
| metrics | 297,000 | 742,000 | **2.50×** |
| turns   | 219,000 | 504,000 | **2.31×** |

| query p50 (ms) | DuckDB | ClickHouse | CH / Duck |
|---|--:|--:|--:|
| calls       | 1.3  | 12.9   | 10.1× |
| summary     | 1.1  | 8.2    | 7.3×  |
| timeseries  | 3.4  | 14.4   | 4.3×  |
| turns       | 6.5  | 16.5   | 2.5×  |
| services    | 41.2 | 1588.3 | 38.5× |

With 50k-row batches ClickHouse **outwrites DuckDB by 2.3–2.9×** on every entity
— its columnar/compressed insert path scales once the per-part overhead is
amortised.

## Results — large scale on a 64-core server

5M calls / 5M metrics / 2M turns, `batch = 50000`. Run on a **64-core slice**
(both engines confined to CPUs 0–63 via `taskset` for the bench process and
`--cpuset-cpus=0-63` for the ClickHouse container) of an internal Linux server
(192 cores / 2 TB RAM), both on the same NVMe, ClickHouse on loopback.
ClickHouse 26.5; DuckDB bundled 1.x. `query-iters = 6`.

| write rows/sec | DuckDB | ClickHouse | CH / Duck |
|---|--:|--:|--:|
| calls   | 21,600  | 79,300  | **3.68×** |
| metrics | 59,600  | 204,200 | **3.43×** |
| turns   | 43,500  | 132,600 | **3.05×** |

| query p50 (ms) | DuckDB | ClickHouse | CH / Duck |
|---|--:|--:|--:|
| calls       | 31.0   | 87.5     | 2.82×  |
| summary     | 15.4   | 57.5     | 3.73×  |
| timeseries  | 37.9   | 70.4     | 1.86×  |
| turns       | 80.1   | 86.9     | **1.09×** |
| services    | 2,065  | 36,813   | **17.8×** |

At 5M rows on 64 cores, **ClickHouse writes 3–3.7× faster** than DuckDB — and the
gap widens vs the 400k run, because DuckDB's per-row write cost grows with table
size while ClickHouse's stays flat. On reads, DuckDB keeps lower latency, but the
gap **narrows with scale** (`query_turns` is essentially tied, 80 vs 87 ms) as
ClickHouse amortises its per-query overhead over more data. The
`query_services` topology query is the glaring exception: **36.8 s** on
ClickHouse vs 2.1 s on DuckDB — its no-JOIN two-step + `FINAL` over `agent_turns`
+ body-sample window function does not scale on ClickHouse and must be reworked
(materialised per-endpoint summary / projection) before a large ClickHouse
deployment relies on the Services page.

## Takeaways

1. **Writes — tune the batch size for ClickHouse.** The sink's default
   `batch_size = 1000` leaves ClickHouse part-creation-bound (~15k rows/s). Raise
   `[storage.sink].batch_size` (e.g. 20k–50k) — or enable ClickHouse
   `async_insert` — when running the ClickHouse backend. At 50k batches it
   overtakes DuckDB **2.3–2.9×** (400k rows) to **3.0–3.7×** (5M rows on 64
   cores); the lead grows with table size because DuckDB's per-row write cost
   rises while ClickHouse's stays flat. DuckDB is insensitive to batch size.
2. **Reads — embedded DuckDB wins, but the gap closes with scale.** For Heron's
   point/small aggregation queries, in-process DuckDB has no per-query or HTTP
   overhead, so it leads on latency. The margin shrinks as data grows (2.5–10× at
   100k–400k rows → `query_turns` essentially tied at 5M). ClickHouse's columnar
   advantage emerges at much larger scale (10⁸–10⁹ rows) and on heavy scans.
3. **`query_services` was the ClickHouse weak spot — now fixed.** The original
   port cost 1.5 s at 400k calls and **36.8 s at 5M** (vs 2.1 s for DuckDB). Root
   cause: a `ROW_NUMBER` body-sample window read every row's ~2 KB body columns
   before filtering, plus `arrayDistinct(groupArray(...))` and `quantileExact`.
   Rewritten (see *Optimisation* below) it now runs **112 ms at 1M / 256 ms at
   3M** — a **46×** speedup at equal data — and scales sub-linearly.

**Guidance.** Use DuckDB for single-node / edge / dev (default). Choose
ClickHouse for high-ingest, multi-node, or very-large-history deployments — and
when you do, raise the sink batch size.

## Optimisation — `query_services`

The first ClickHouse port of `query_services` mirrored the DuckDB SQL too
literally. Three changes (in `h-storage-clickhouse/src/services.rs`), each
verified by server-side `elapsed` on a 1M-row single-endpoint dataset:

| piece | before | after | change |
|---|--:|--:|---|
| body/header sample | 5003 ms | ~80 ms | `ROW_NUMBER` window over all rows' bodies → `id IN (SELECT id … LIMIT 5 BY server_ip,server_port)` — the inner subquery reads no body columns; the outer fetches bodies for only the ~5 recent ids per endpoint |
| distinct models / paths / wire_apis | 175 ms | 34 ms | `arraySlice(arrayDistinct(groupArray(col)),1,N)` → `groupUniqArray(N)(col)` (streaming capped-distinct; no giant intermediate array) |
| p95 latency | (in agg) | (in agg) | `quantileExact(0.95)` → `toFloat64(quantileTDigest(0.95))` (streaming t-digest; `quantileExact` holds every value). Note `quantileTDigest` returns `Float32`, hence `toFloat64`. |

End-to-end `query_services` (same local CH, same data): **5129 ms → 112 ms at
1M** (46×); **256 ms at 3M**. The body-sample window was the dominant cost; the
`id IN (subquery)` form is an uncorrelated subquery (not a JOIN), so it keeps the
no-JOIN read rule. `quantileTDigest` is approximate (~1–2 % on tails) — already
the accuracy the DuckDB backend accepts for its `approx_quantile` rollups.

> `query_services_topology` shares the optimised `fetch_app_samples` and node
> aggregation, so it benefits too; its remaining cost is reading every turn in
> the window to build the graph, which is inherent to the no-JOIN edge
> construction.
