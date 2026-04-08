# CLAUDE.md

## Project Overview

TokenScope is an LLM API performance monitoring system that analyzes network traffic to measure and diagnose LLM inference performance. Deployed on the **LLM provider's server side** (post-TLS termination, plaintext HTTP), it serves ops, dev, and business teams.

**Data acquisition:**
- Local NIC capture via libpcap
- Remote packet ingestion via ZMQ from [cloud-probe](https://github.com/Netis/cloud-probe)

**Supported LLM providers:** OpenAI, Anthropic, Azure OpenAI, local deployments (vLLM/Ollama, OpenAI-compatible)

**Key metrics:** TTFB, TPOT (time per output token), E2E latency, token throughput (tokens/s), error rates, concurrency

## Tech Stack

### Backend — Rust

- **Async runtime:** Tokio
- **Web framework:** Axum
- **Packet capture:** pcap crate (libpcap)
- **ZMQ:** zeromq ([zmq.rs](https://github.com/zeromq/zmq.rs), pure Rust)
- **HTTP parsing:** httparse (zero-copy)
- **Serialization:** serde + serde_json
- **Storage:** sqlx (SQLite / PostgreSQL), clickhouse-rs (ClickHouse) — pluggable backend via trait
- **Config:** config crate (TOML)
- **Logging:** tracing + tracing-subscriber
- **CLI:** clap

### Frontend — React SPA

- React + TypeScript
- shadcn/ui + Tailwind CSS
- Bun + Vite
- React Router

## Repository Structure

```
TokenScope/
├── CLAUDE.md
├── docs/design/                 # Module design docs (AI dev reference)
├── server/                      # Rust backend (Cargo workspace)
│   ├── Cargo.toml               # workspace root + workspace.package + workspace.dependencies
│   ├── ts-common/               # Shared config, error types
│   ├── ts-capture/              # libpcap + cloud-probe ZMQ receiver → RawPacket
│   ├── ts-protocol/             # net (L2-L4) + http (HTTP/SSE) parsing → HttpExchange
│   ├── ts-llm/                  # Provider registry + extractors + LoopTracker → LlmRequest
│   ├── ts-metrics/              # Sliding-window aggregation → LlmMetric
│   ├── ts-storage/              # StorageBackend trait + SQLite/PG/ClickHouse + write buffer
│   ├── ts-api/                  # Axum REST API + WebSocket
│   ├── app/
│   │   └── tokenscope/          # Binary entry crate
│   └── config/
│       └── default.toml
├── web/                         # React frontend (Bun + Vite)
├── deploy/                      # Dockerfiles, docker-compose (future)
└── scripts/                     # Dev/build helper scripts
```

Pipeline: capture → protocol (link-layer + TCP reassembly + HTTP/SSE parsing) → llm (provider extraction + LoopTracker) → metrics (aggregation) + storage (DB write). Each stage connected by tokio mpsc channels.

See [docs/design/architecture.md](docs/design/architecture.md) for detailed design decisions.

## Storage

Three entities: `llm_loops` (agent loop), `llm_requests` (per-call detail + full body), `llm_metrics` (pre-aggregated time-series). Relation: `llm_loops 1─N llm_requests`. Pluggable backends:

| Backend | Use case |
|---------|----------|
| SQLite | Single-node, POC, edge |
| PostgreSQL | Mid-scale production (+ TimescaleDB optional) |
| ClickHouse | Large-scale, high-throughput columnar analytics |

See [docs/design/schema.md](docs/design/schema.md) for full schema design.

## Frontend Pages

- Dashboard — cluster-level realtime overview
- Model Analysis — compare TTFB/TPOT/throughput across models
- Tenant Analysis — per-API-key usage and performance
- Request List — filterable detail table
- Request Detail — waterfall chart + token flow curve
- Settings — runtime configuration
