#!/usr/bin/env bash
set -euo pipefail

GREEN='\033[0;32m'
RED='\033[0;31m'
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
    echo "✅ Code Quality"
    echo "   just quality all        Run format + lint + typecheck (both sides)"
    echo "   just quality format     rustfmt + (no-op for TS)"
    echo "   just quality lint       cargo clippy + bun run lint"
    echo "   just quality typecheck  cargo check + tsc --noEmit"
    echo "   just quality rs         All Rust checks only"
    echo "   just quality ts         All TypeScript checks only"
}

rs_format() { echo -e "${BLUE}[rs] cargo fmt${NC}"; (cd server && cargo fmt); }
rs_lint()   { echo -e "${BLUE}[rs] cargo clippy${NC}"; (cd server && cargo clippy --all-targets -- -D warnings); }
rs_types()  { echo -e "${BLUE}[rs] cargo check${NC}"; (cd server && cargo check --all-targets); }

ts_lint()  { echo -e "${BLUE}[ts] bun run lint${NC}"; (cd console && bun run lint); }
ts_types() { echo -e "${BLUE}[ts] tsc --noEmit${NC}"; (cd console && bunx tsc --noEmit); }

run_format()    { rs_format; }
run_lint()      { rs_lint; ts_lint; }
run_typecheck() { rs_types; ts_types; }

run_rs() { rs_format; rs_lint; rs_types; }
run_ts() { ts_lint; ts_types; }

run_all() {
    local failed=0
    run_format    || failed=1
    run_lint      || failed=1
    run_typecheck || failed=1
    if [ $failed -eq 0 ]; then
        echo -e "${GREEN}All quality checks passed${NC}"
    else
        echo -e "${RED}Some quality checks failed${NC}"
        exit 1
    fi
}

ACTION="${1:-help}"

case "$ACTION" in
    format|fmt) run_format ;;
    lint) run_lint ;;
    typecheck|types|tc) run_typecheck ;;
    rs|rust|server) run_rs ;;
    ts|typescript|console) run_ts ;;
    all) run_all ;;
    help|--help|-h) show_help ;;
    *) echo "Unknown: $ACTION. Run 'just quality' for help."; exit 1 ;;
esac
