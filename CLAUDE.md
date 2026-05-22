# CLAUDE.md

## Core Principles

1. **Occam's Razor** — Prefer simple solutions; don't add abstractions until needed.
2. **Code Quality** — Modular, single source of truth, explicit over clever, types as documentation.
3. **Documentation** — CLAUDE.md is the entry point (keep concise). Commands over prose. Update docs with code.
4. **Project Structure** — `justfile` as command runner, `scripts/` for complex logic, `project.yaml` for metadata.
5. **PR Hygiene** — Never leak internal LAN, host, app, deployment, screenshot, log, URL, or environment details into PR titles, bodies, comments, or review requests. Scrub private IPs, internal hostnames, internal app names, usernames, and machine-specific paths before creating PR text.

These apply to every artifact in this repo — code, docs, and CLAUDE.md itself.

## Project Overview

TokenScope is an LLM API performance monitoring system that analyzes network traffic to measure and diagnose LLM inference performance. Deployed on the **LLM provider's server side** (post-TLS termination, plaintext HTTP), it serves ops, dev, and business teams.

**Data acquisition:**
- Local NIC capture via libpcap
- Remote packet ingestion via ZMQ from [cloud-probe](https://github.com/Netis/cloud-probe)

**Supported LLM providers:** OpenAI, Anthropic, Azure OpenAI, Gemini, local deployments (vLLM/Ollama, OpenAI-compatible)

**Key metrics:** TTFT, E2E Latency, TPOT, Call Rate, Token Throughput, Active Calls, Call Error Rate, Cache Hit Ratio

## Tech Stack

### Backend — Rust

- **Async runtime:** Tokio
- **Web framework:** Axum
- **Packet capture:** pcap crate (libpcap)
- **ZMQ:** zeromq ([zmq.rs](https://github.com/zeromq/zmq.rs), pure Rust)
- **HTTP parsing:** httparse (zero-copy)
- **Serialization:** serde + serde_json
- **Storage:** duckdb-rs (DuckDB), sqlx (PostgreSQL), clickhouse-rs (ClickHouse) — pluggable backend via trait
- **Config:** config crate (TOML)
- **Logging:** tracing + tracing-subscriber
- **CLI:** clap

### Frontend — React SPA

- React + TypeScript
- shadcn/ui + Tailwind CSS
- Bun + Vite
- React Router
- TanStack Query (server state: API caching, polling, retry)
- Zustand (client state: UI state, filters, preferences)

**Styling rule:** Always use Tailwind CSS utility classes. Never write raw CSS except in `index.css` for Tailwind directives and CSS variable definitions.

## Repository Structure

```
TokenScope/
├── CLAUDE.md
├── docs/design/                 # Module design docs (AI dev reference)
├── server/                      # Rust backend (Cargo workspace)
│   ├── Cargo.toml               # workspace root + workspace.package + workspace.dependencies
│   ├── ts-common/               # Shared config, error types
│   ├── ts-capture/              # libpcap + cloud-probe ZMQ receiver → RawPacket
│   ├── ts-protocol/             # net (L2-L4) + http (HTTP/SSE) parsing
│   ├── ts-llm/                  # Wire-API detection + extractors → LlmCall
│   ├── ts-turn/                 # Agent profiles + state machine → AgentTurn
│   ├── ts-metrics/              # Sliding-window aggregation → LlmMetric
│   ├── ts-storage/              # StorageBackend trait + DuckDB/PG/ClickHouse + write buffer
│   ├── ts-api/                  # Axum REST API
│   ├── app/
│   │   └── tokenscope/          # Binary entry crate
│   └── config/
│       └── default.toml
├── console/                     # React frontend (Bun + Vite)
├── deploy/                      # Dockerfiles, docker-compose (future)
└── scripts/                     # Dev/build helper scripts
```

Pipeline: capture → flow dispatcher (hash by flow key) → N parallel workers (protocol + llm) → turn tracker + metrics (aggregation) + storage (DB write). Flow sharding ensures same-connection packets stay in one worker. See architecture.md for details.

See [docs/design/01-architecture.md](docs/design/01-architecture.md) for detailed design decisions.
See [docs/design/04-turn.md](docs/design/04-turn.md) for Turn (agent interaction) tracking design.

## Storage

Three entities: `agent_turns` (agent turn), `llm_calls` (per-call detail + full body), `llm_metrics` (pre-aggregated time-series). Relation: `agent_turns 1─N llm_calls`. Pluggable backends:

**Query rule — no JOIN.** Read-path SQL MUST NOT use any `JOIN`. Cross-entity reads are split into multiple point lookups: e.g. fetch `call_ids` from `agent_turns` by PK, then `SELECT ... FROM llm_calls WHERE id IN (?, ?, ...)`. See `query_turn_calls` in `ts-storage/src/duckdb.rs` for the canonical pattern. This keeps queries uniformly cheap across DuckDB/PG/ClickHouse and avoids planner surprises at scale.

| Backend | Use case |
|---------|----------|
| DuckDB | Default, single-node, dev, edge (embedded, single-file) |
| PostgreSQL | Mid-scale production (+ TimescaleDB optional) |
| ClickHouse | Large-scale, high-throughput columnar analytics |

See [docs/design/07-schema.md](docs/design/07-schema.md) for full schema design.
