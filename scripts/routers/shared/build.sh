#!/usr/bin/env bash
set -euo pipefail

GREEN='\033[0;32m'
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
    echo "📦 Build"
    echo "   just build all        Build single binary with embedded console"
    echo "   just build server     Build Rust backend only (no embed)"
    echo "   just build console    Build React frontend only"
}

run_console() {
    echo -e "${BLUE}Building console (bun + vite)...${NC}"
    cd console && bun install && bun run build
    echo -e "${GREEN}Console built: console/dist${NC}"
}

run_server() {
    echo -e "${BLUE}Building server (cargo release)...${NC}"
    cd server && cargo build --release
    echo -e "${GREEN}Server built: server/target/release/heron${NC}"
}

run_all() {
    run_console
    cd "$PROJECT_ROOT"
    echo -e "${BLUE}Building server with embedded console...${NC}"
    cd server && cargo build --release --features console
    echo -e "${GREEN}All built: server/target/release/heron${NC}"
}

# No subcommand → print help AND exit non-zero. Fails closed so CI / scripts
# calling `just build` cannot silently skip the build and get exit 0.
if [ $# -eq 0 ]; then
    show_help
    echo ""
    echo "error: 'just build' requires a subcommand (e.g. 'just build all')" >&2
    exit 2
fi

ACTION="$1"

case "$ACTION" in
    all|release) run_all ;;
    server|backend|rs) run_server ;;
    console|frontend|ui|ts) run_console ;;
    help|--help|-h) show_help ;;
    *) echo "Unknown: $ACTION. Run 'just build help' for options."; exit 1 ;;
esac
