# CLAUDE.md

## Core Principles

1. **Occam's Razor** — Prefer simple solutions; don't add abstractions until needed.
2. **Code Quality** — Modular, single source of truth, explicit over clever, types as documentation.
3. **Documentation** — CLAUDE.md is the entry point (keep concise). Commands over prose. Update docs with code.
4. **Project Structure** — `justfile` as command runner, `scripts/` for complex logic, `project.yaml` for metadata.
5. **PR Hygiene** — Never leak internal LAN, host, app, deployment, screenshot, log, URL, or environment details into any outbound surface: PR titles/bodies, inline review comments, issue bodies, commit messages destined for public history, and any text submitted via `gh` to GitHub. Scrub private IPs, internal infrastructure hostnames (including the project's specific runner/staging machine names — refer to them generically as "the self-hosted runner" or "the staging VM"), internal app/team names, usernames, and machine-specific paths before sending. This rule also applies to `CLAUDE.md` itself and every file in the repo: never enumerate the actual sensitive identifiers here as "examples"; refer to them by category only.

These apply to every artifact in this repo — code, docs, and CLAUDE.md itself.

## Project Overview

Heron is an LLM API performance monitoring system that analyzes network traffic to measure and diagnose LLM inference performance. Deployed on the **LLM provider's server side** (post-TLS termination, plaintext HTTP), it serves ops, dev, and business teams.

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
heron/
├── CLAUDE.md
├── docs/design/                 # Module design docs (AI dev reference)
├── server/                      # Rust backend (Cargo workspace)
│   ├── Cargo.toml               # workspace root + workspace.package + workspace.dependencies
│   ├── h-common/                # Shared config, error types
│   ├── h-capture/               # libpcap + cloud-probe ZMQ receiver → RawPacket
│   ├── h-protocol/              # net (L2-L4) + http (HTTP/SSE) parsing
│   ├── h-llm/                   # Wire-API detection + extractors → LlmCall
│   ├── h-turn/                  # Agent profiles + state machine → AgentTurn
│   ├── h-metrics/               # Sliding-window aggregation → LlmMetric
│   ├── h-storage/               # StorageBackend trait + DuckDB/PG/ClickHouse + write buffer
│   ├── h-storage-duckdb/        # DuckDB implementation of StorageBackend
│   ├── h-storage-clickhouse/    # ClickHouse implementation of StorageBackend
│   ├── h-api/                   # Axum REST API
│   ├── app/
│   │   └── heron/               # Binary entry crate
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

**Query rule — no JOIN.** Read-path SQL MUST NOT use any `JOIN`. Cross-entity reads are split into multiple point lookups: e.g. fetch `call_ids` from `agent_turns` by PK, then `SELECT ... FROM llm_calls WHERE id IN (?, ?, ...)`. See `query_turn_calls` in `h-storage-duckdb/src/turns.rs` for the canonical pattern. This keeps queries uniformly cheap across DuckDB/PG/ClickHouse and avoids planner surprises at scale.

| Backend | Use case |
|---------|----------|
| DuckDB | Default, single-node, dev, edge (embedded, single-file) |
| PostgreSQL | Mid-scale production (+ TimescaleDB optional) |
| ClickHouse | Large-scale, high-throughput columnar analytics |

See [docs/design/07-schema.md](docs/design/07-schema.md) for full schema design.

## Quality & release pipeline

Code reaches production — and a release — through a layered, gated chain. Each
class of past failure has a deterministic gate before it can ship.

**Per-PR (CI, before merge)** — on a dedicated CI runner pool:
- `cargo test --workspace`, plus a `--features fault-injection` run whose
  `concurrent_tests` drive **fault injection under sustained concurrent write
  load** (DuckDB FATAL/invalidate + ENOSPC/`DiskFull`): a write either commits
  or returns `Err` — never silently lost, never partial — and
  `reopen_all_connections` recovers with no lost rows.
- Schema-migration tests over synthesized legacy DB shapes.
- Lint gates: referenced-secret provisioning, secret-value sanity,
  validated-constructor scoping, and an internal-infra **leakage** gate (no
  private IP / key material / machine path in any tracked file).
- `cargo bench -p h-protocol --no-run` — criterion hot-path benches compiled so
  they can't bitrot (timings are too noisy on shared runners to gate on).
- Stdlib unit tests for the staging-soak, longevity, and distributed-soak judges.
- Wire-equivalence + transport tests for the distributed eBPF topology
  (`cargo test --workspace`): the **differential keystone** replays the corpus
  locally vs through the probe→mTLS→central path and asserts identical turns/
  calls (proving the central is byte-for-byte the local source); plus
  redaction-over-wire and many-probe scale/churn/backpressure/version-skew.
- An automated PR-review agent reviews the diff.

**On merge to `main`** — `ci → deploy-staging → { staging-soak, ebpf-soak,
distributed-soak }` (the three soaks run in parallel, each stamping a commit
status):
- **staging-soak** replays a known pcap corpus through the freshly deployed
  binary on the staging VM and asserts parse/pairing/turn/persistence
  invariants, then runs a **rate-controlled sustained-load soak** (bounded
  queues, stable RSS, zero drops/flush-errors). Both compare against a rolling
  dual-binary known-good — a too-tight environment surfaces as `harness_broken`,
  not a false candidate-reject. On pass it stamps a `staging-soaked` commit
  status.
- **ebpf-soak** proves on-host SSL-uprobe capture end-to-end on the staging VM →
  `ebpf-soaked`.
- **distributed-soak** runs the large-scale distributed-capture soak — N
  synthetic probes (looped pcap-file source over mTLS) → one isolated central —
  asserting central health (queues/RSS/flush) + every probe correctly attributed
  → `distributed-soaked`. (B2: a small real-eBPF multi-VM fidelity check reuses
  the ebpf-soak mechanism.)

**Before production** — `{ staging-soak, ebpf-soak, distributed-soak } →
[manual approval] → deploy-prod`:
- deploy-prod builds on the prod host, restarts the service, gates on a health
  check with **automatic rollback**, and supersedes older waiting approvals so
  the queue can't wedge. The post-approval gate requires all three commit
  statuses (`staging-soaked` + `ebpf-soaked` + `distributed-soaked`) green; the
  load soak is **enforcing** — a regression vs known-good blocks the deploy.

**Before a release** — push a `v*` tag → `release.yml`:
- A `gate` job refuses to build/publish unless the tagged commit carries passing
  `staging-soaked` + `ebpf-soaked` + `distributed-soaked` statuses, so the
  multi-arch binaries are never cut from un-soaked code. Cutting a release: `just bump` (the `VERSION` file is the SSOT
  for the binary's embedded version) → tag `v<version>` on the soaked commit →
  `release.yml` builds the binaries and creates the Release, keyed by the tag,
  with notes from the matching `CHANGELOG.md` section. (Tag/`VERSION` agreement
  is by the `just bump` → tag convention, not yet enforced in the workflow.)

**Out of band** — a **nightly longevity soak** (a timer on the staging VM) runs
the load soak for hours, tracking RSS + on-disk DB size to catch slow leaks /
unbounded growth (the checkpoint-bloat class), and files a scrubbed, deduplicated
issue on regression.
