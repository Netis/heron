# Heron justfile
# Router pattern: recipes dispatch to scripts/routers/shared/*.sh
# Run `just help` for the menu, or `just <router>` for per-router detail.

set shell := ["bash", "-cu"]

project := "Heron"
version := `cat VERSION 2>/dev/null || echo "dev"`

# Default: show help
default:
    @just help

# Show top-level help with popular commands
help:
    @echo ""
    @echo "📡 {{project}} v{{version}} — LLM API performance monitor"
    @echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    @echo ""
    @echo "🚀 Development"
    @echo "   just dev server        Run backend (config/default.toml)"
    @echo "   just dev console       Run Vite dev server"
    @echo "   just dev setup         Install cargo + bun deps"
    @echo ""
    @echo "📦 Build"
    @echo "   just build all         Single binary with embedded console"
    @echo "   just build server      Rust release build"
    @echo "   just build console     Frontend production bundle"
    @echo ""
    @echo "✅ Quality"
    @echo "   just quality all       Format + lint + typecheck (Rust & TS)"
    @echo "   just quality rs        Rust only (fmt + clippy + check)"
    @echo "   just quality ts        TypeScript only (lint + tsc)"
    @echo ""
    @echo "🧪 Testing"
    @echo "   just test all          Run cargo test (all crates)"
    @echo "   just test crate <name> Test a single workspace crate"
    @echo ""
    @echo "🌲 Worktrees"
    @echo "   just wt add <name>     Create worktree + feature branch"
    @echo "   just wt list           List worktrees"
    @echo "   just wt merge <name>   Cherry-pick back to current branch"
    @echo ""
    @echo "🔭 Tools"
    @echo "   just loc               Lines of code dashboard"
    @echo ""
    @echo "⚡ Meta"
    @echo "   just version           Show version"
    @echo "   just bump <kind>       Bump VERSION + sync Cargo.toml/package.json"
    @echo "   just <router>          Detail help (e.g. 'just build')"
    @echo ""

# Show version
version:
    @echo "{{project}} v{{version}}"

# =============================================================================
# Routers — `just <name>` (no args) prints that router's detail help.
# =============================================================================

# Build (console, server, or full binary)
build *args:
    @bash scripts/routers/shared/build.sh {{args}}

# Development (run server/console, setup, clean)
dev *args:
    @bash scripts/routers/shared/dev.sh {{args}}

# Code quality (format, lint, typecheck)
quality *args:
    @bash scripts/routers/shared/quality.sh {{args}}

# Testing (cargo test, bun test, per-crate)
test *args:
    @bash scripts/routers/shared/test.sh {{args}}

# Benchmarking (criterion hot-path micro-benches — h-protocol/benches/hot_paths.rs)
#   just bench                 run all hot-path benches
#   just bench --no-run        compile only (the CI bitrot gate)
bench *args:
    @cd server && cargo bench -p h-protocol {{args}}

# Worktree management (add, list, merge, remove)
wt *args:
    @bash scripts/routers/shared/wt.sh {{args}}

# Version bump (VERSION is SSOT; syncs Cargo.toml + package.json)
bump *args:
    @bash scripts/routers/shared/bump.sh {{args}}

# Lines of code dashboard
loc *args:
    @bash scripts/routers/shared/loc.sh {{args}}

# Storage benchmark (ClickHouse vs DuckDB write throughput + read latency).
# Run on the host where ClickHouse is on loopback. Env: CLICKHOUSE_URL, CALLS, …
bench-storage:
    @bash scripts/bench-storage.sh

# 🧪 PCAP regression corpus (test | bless | lint)
#   just corpus test    Replay corpus/*.pcap, assert goldens (skips absent/LFS-pointer)
#   just corpus bless   Regenerate goldens (review the diff before committing)
#   just corpus lint    Manifest consistency + binary-payload leakage scan
corpus action="test":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{action}}" in
      test)  cd server && cargo test -p h-turn --test corpus_golden -- --nocapture ;;
      bless) cd server && HERON_BLESS_GOLDENS=1 cargo test -p h-turn --test corpus_golden -- --nocapture ;;
      lint)  bash scripts/lint/check-pcap-corpus.sh ;;
      *) echo "usage: just corpus [test|bless|lint]"; exit 2 ;;
    esac
