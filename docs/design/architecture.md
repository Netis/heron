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
├── web/                             # React frontend
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
│   │   ├── hooks/                   # Data fetching & WebSocket hooks
│   │   ├── lib/                     # API client, utilities
│   │   └── types/                   # TypeScript types (mirrors backend models)
│   └── components.json              # shadcn/ui config
│
├── deploy/                          # Deployment configs (future)
│   ├── docker/
│   │   ├── Dockerfile.server
│   │   └── Dockerfile.web
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

### Why `server/` + `web/` side-by-side (not nested)?

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

Each capture source runs its own independent pipeline. Pipelines converge at the storage layer.

```
Per-source pipeline:

capture ──▶ protocol::net (link-layer strip → IP/TCP parse → TCP reassembly)
                    │
                    ▼  TCP byte stream + timestamps
             protocol::http (HTTP req/resp parsing + SSE framing)
                    │
                    ▼  HttpExchange
              llm (provider detection → extractor → LlmRequest)
                    │
                    ▼
              llm::LoopTracker (sets LlmRequest.loop_id, produces LlmLoop)
                    │
                    ├──▶ LlmRequest + LlmLoop ──▶ storage::buffer ──▶ DB
                    │
                    └──▶ LlmRequest ──▶ metrics::Aggregator
                                              │
                                              ▼  (on window close)
                                         LlmMetric ──▶ storage::buffer ──▶ DB

Multiple sources (pcap, cloud-probe) run in parallel,
each with its own pipeline, all writing to shared storage.
```

Each stage runs as a Tokio task (capture::pcap uses `std::thread`). Stages are connected by `tokio::sync::mpsc` channels with bounded capacity, providing natural backpressure propagation.

## Crate Responsibilities

| Crate | Responsibility | Key Types |
|-------|---------------|-----------|
| `ts-common` | Shared config (TOML), unified error type, global constants | `Config`, `AppError` |
| `ts-capture` | libpcap packet capture + cloud-probe ZMQ receiver | `RawPacket` |
| `ts-protocol` | Link-layer stripping, IP/TCP parsing, TCP reassembly, HTTP/1.1 parsing, SSE framing | `FlowKey`, `TcpStream`, `HttpRequest`, `HttpResponse`, `SseEvent`, `HttpExchange` |
| `ts-llm` | Provider auto-detection, registry + extractor pattern, agent loop tracking | `ProviderRegistry`, `ProviderExtractor` trait, `LlmRequest`, `LoopTracker`, `LlmLoop` |
| `ts-metrics` | Sliding-window aggregation of LlmRequest into LlmMetric (P50/P95/P99 via t-digest) | `MetricsAggregator`, `WindowBucket`, `LlmMetric` |
| `ts-storage` | StorageBackend trait + SQLite/PostgreSQL/ClickHouse implementations, write buffer with batch flush | `StorageBackend` trait, `WriteBuffer` |
| `ts-api` | Axum HTTP routes + WebSocket realtime push, serves frontend static files in production | REST endpoints, WS handlers |

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
