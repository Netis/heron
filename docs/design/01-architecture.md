# Architecture

## Monorepo Layout

```
TokenScope/
├── CLAUDE.md                        # AI context — always loaded
├── docs/
│   ├── design/                      # Module design docs (AI dev reference)
│   ├── user/                        # User-facing docs (deployment, usage)
│   └── api/                         # API reference (future)
│
├── server/                          # Rust backend (Cargo workspace)
│   ├── Cargo.toml                   # workspace root + workspace.package + workspace.dependencies
│   ├── Cargo.lock
│   ├── ts-common/                   # Shared types and utilities
│   ├── ts-capture/                  # Data acquisition
│   ├── ts-protocol/                 # Network + HTTP protocol parsing
│   ├── ts-llm/                      # LLM semantic extraction
│   ├── ts-metrics/                  # Sliding-window aggregation
│   ├── ts-storage/                  # Pluggable storage
│   ├── ts-api/                      # REST API
│   ├── app/
│   │   └── tokenscope/             # Binary entry crate
│   │       ├── Cargo.toml
│   │       └── src/main.rs          # config → pipeline → API server
│   └── config/
│       └── default.toml
│
├── console/                         # React frontend
│   ├── package.json
│   ├── bun.lockb
│   ├── vite.config.ts
│   ├── tailwind.config.ts
│   ├── tsconfig.json
│   ├── index.html
│   ├── public/
│   ├── src/
│   │   ├── main.tsx
│   │   ├── routes.tsx               # React Router route definitions
│   │   ├── components/
│   │   │   ├── ui/                  # shadcn/ui components
│   │   │   └── charts/             # ECharts/Recharts wrappers
│   │   ├── pages/                   # Route-level page components
│   │   ├── hooks/                   # Data fetching hooks
│   │   ├── lib/                     # API client, utilities
│   │   └── types/                   # TypeScript types (mirrors backend models)
│   └── components.json              # shadcn/ui config
│
├── deploy/                          # Deployment configs (future)
│   ├── docker/
│   │   ├── Dockerfile.server
│   │   └── Dockerfile.console
│   └── docker-compose.yml
│
└── scripts/                         # Dev/build helper scripts
    ├── dev.sh                       # Start frontend + backend together
    └── build.sh                     # Production build
```

## Workspace Organization

Following rpktminer conventions:
- **`workspace.package`**: version and edition defined once at workspace level
- **`workspace.dependencies`**: all third-party dependency versions centralized in root Cargo.toml; each crate uses `dep.workspace = true`
- **`ts-` prefix**: all library crates prefixed to avoid name collisions with crates.io (e.g. `ts-http` vs `http`)
- **`app/*` wildcard**: binary crates separated from library crates under `app/`
- **`members = ["ts-*", "app/*"]`**

## Design Decisions

### Why `server/` + `console/` side-by-side (not nested)?

Rust workspace and Node project have completely separate toolchains (cargo vs bun), config files (.toml vs .json), and IDE support. Keeping them as sibling directories avoids conflicts and lets each side own its own lint/format/build config.

### Why `ts-` prefix?

Crate names like `net`, `http`, `storage` collide with existing crates.io packages. The `ts-` prefix (TokenScope) keeps names short while avoiding conflicts. This matches rpktminer's `pktm-` convention.

### Why `app/` directory for binaries?

Separates the thin binary entry point from library crates. Future binaries (CLI tools, benchmarks) go under `app/` without cluttering the library namespace.

### Why merge net + http into `ts-protocol`?

`net` and `http` form a tight pipeline (net is only consumed by http). Merging into one crate with internal modules (`protocol::net`, `protocol::http`) reduces package count while keeping the logical separation via modules. The SSE transport vs semantic split is preserved — `ts-protocol` handles transport, `ts-llm` handles semantics.

### Why `ts-metrics` as a separate crate?

Metrics aggregation (sliding-window counters + t-digest sketches) is pure computation with no DB dependency. Keeping it separate from `ts-storage` ensures clean testability and a clear dependency direction: `ts-metrics` produces `LlmMetric`, `ts-storage` consumes it.

### Why `docs/design/` for AI dev reference?

CLAUDE.md is loaded every conversation but must stay concise. `docs/design/` holds detailed design decisions, schema definitions, and interface contracts that AI reads on-demand when implementing specific modules.

## Data Pipeline Architecture

Each capture source runs its own independent sub-pipeline — dispatcher → protocol → llm → turn → **metrics** — all the way through aggregation. Only the storage sink is shared. Giving every capture its own metrics aggregator means each source owns its event-time watermark, so inter-source clock skew (cloud-probe vs. local pcap, different hosts) cannot re-open already-flushed windows and produce duplicate rows. Every emitted `LlmMetric` carries a `source_id` (today = capture-source index) so the sink / query layer can merge per-source rows explicitly at read time.

```
Per-source pipeline:

capture ──▶ packet_parser (extract flow key from IP/TCP headers)
                    │
                    ▼
             flow_dispatcher (hash(flow_key) % N)
                    │
          ┌─────────┼─────────┐
          ▼         ▼         ▼
      worker 0  worker 1  worker N
          │         │         │
          │   Each worker runs:
          │     protocol::net  (TCP reassembly for assigned flows)
          │     protocol::http (HTTP/SSE parsing)
          │     llm            (wire-API detection + extraction)
          │       ├──▶ CallStart event ──▶ metrics::Aggregator(source_id)
          │       └──▶ LlmCall            (per-capture; own watermark)
          └─────────┼─────────┘
                    │
            per-capture LlmMetric ──┐
            per-capture LlmCall    ─┤
            per-capture AgentTurn    ─┤
                                    ▼
                             storage::WriteBuffer (shared)
                                    │
                                    ▼
                                   DB

Multiple sources (pcap, cloud-probe) run in parallel, each with its own
aggregator group; the sink stamps source_id on every metrics row so
downstream queries can merge streams back together at read time.
```

### Flow Sharding

TCP reassembly is stateful per-flow — packets from the same connection must be processed in order. But different flows are independent. Flow sharding distributes packets across N parallel workers based on `hash(src_ip, dst_ip, src_port, dst_port) % N`, so each worker handles a disjoint subset of flows.

The `packet_parser` stage is lightweight: it only parses IP/TCP headers to extract the flow key for dispatching. Full protocol decoding happens inside each worker.

Worker count is configurable (default: number of CPU cores or a fixed value like 4).

### Threading Model

- **capture::pcap**: dedicated OS thread (`std::thread`) — `next_packet()` is blocking
- **capture::cloud-probe**: Tokio task — native async ZMQ
- **packet_parser + flow_dispatcher**: Tokio task — lightweight header parsing + channel dispatch
- **workers**: each worker is a Tokio task — runs the full protocol → llm pipeline for its flow subset
- **metrics::Aggregator**: one Tokio task group *per capture source* — each owns an independent event-time watermark, keyed by `source_id`; all emit into the shared storage sink
- **storage::WriteBuffer**: single Tokio task — batches writes from all workers

Stages are connected by `tokio::sync::mpsc` channels with bounded capacity, providing natural backpressure propagation.

## Crate Responsibilities

| Crate | Responsibility | Key Types |
|-------|---------------|-----------|
| `ts-common` | Shared config (TOML), unified error type, global constants | `Config`, `AppError` |
| `ts-capture` | libpcap packet capture + cloud-probe ZMQ receiver | `RawPacket` |
| `ts-protocol` | Flow-key extraction, flow dispatcher, link-layer stripping, IP/TCP parsing, TCP reassembly, HTTP/1.1 parsing, SSE framing | `FlowKey`, `FlowDispatcher`, `TcpStream`, `HttpRequest`, `HttpResponse`, `SseEvent` |
| `ts-llm` | Wire-API auto-detection, registry + extractor pattern | `WireApiRegistry`, `WireApi` trait, `LlmCall` |
| `ts-metrics` | Sliding-window aggregation of LlmCall into LlmMetric (P50/P95/P99 via t-digest) | `MetricsAggregator`, `WindowBucket`, `LlmMetric` |
| `ts-storage` | StorageBackend trait + DuckDB/PostgreSQL/ClickHouse implementations, write buffer with batch flush | `StorageBackend` trait, `WriteBuffer` |
| `ts-api` | Axum HTTP routes, serves frontend static files in production | REST endpoints |

## Crate Dependency Graph

```
ts-common ◀───────────────────────────────────────────────────┐
  ▲                                                            │
  ├── ts-capture                                               │
  ├── ts-protocol  ◀── ts-capture                              │
  ├── ts-llm       ◀── ts-protocol                             │
  ├── ts-metrics   ◀── ts-llm                                  │
  ├── ts-storage   ◀── ts-llm, ts-metrics                      │
  └── ts-api       ◀── ts-storage, ts-metrics, ts-llm          │
                                                               │
app/tokenscope ──▶ all crates ────────────────────────────────┘
```

Dependencies flow left-to-right through the pipeline. No circular dependencies. `ts-common` is depended on by all. `app/tokenscope` wires everything together.
