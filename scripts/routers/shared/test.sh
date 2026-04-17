#!/usr/bin/env bash
set -euo pipefail

BLUE='\033[0;34m'
NC='\033[0m'

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$PROJECT_ROOT" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
fi
cd "$PROJECT_ROOT"

show_help() {
    echo ""
    echo "🧪 Testing"
    echo "   just test all             Run cargo test (all workspace crates)"
    echo "   just test rs [filter]     cargo test (optional filter)"
    echo "   just test ts              bun test in console/"
    echo "   just test crate <name>    Test a single workspace crate"
}

run_rs() {
    echo -e "${BLUE}[rs] cargo test${NC}"
    (cd server && cargo test "$@")
}

run_ts() {
    echo -e "${BLUE}[ts] bun test${NC}"
    (cd console && bun test "$@")
}

run_crate() {
    local name="${1:-}"
    if [ -z "$name" ]; then echo "Usage: just test crate <name>" >&2; exit 1; fi
    shift
    echo -e "${BLUE}[rs] cargo test -p $name${NC}"
    (cd server && cargo test -p "$name" "$@")
}

ACTION="${1:-help}"
shift 2>/dev/null || true

case "$ACTION" in
    all) run_rs "$@" ;;
    rs|rust|server) run_rs "$@" ;;
    ts|typescript|console) run_ts "$@" ;;
    crate|pkg|p) run_crate "$@" ;;
    help|--help|-h) show_help ;;
    *) echo "Unknown: $ACTION. Run 'just test' for help."; exit 1 ;;
esac
