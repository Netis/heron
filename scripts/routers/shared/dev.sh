#!/usr/bin/env bash
set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
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
    echo "🚀 Development"
    echo "   just dev server       Run backend with config/default.toml"
    echo "   just dev console      Run Vite dev server (proxies API)"
    echo "   just dev setup        Install all dependencies (cargo + bun)"
    echo "   just dev clean        Clean build artifacts (target/, dist/)"
}

run_server() {
    echo -e "${BLUE}Starting backend (cargo run)...${NC}"
    cd server && cargo run --bin heron -- -c config/default.toml
}

run_console() {
    echo -e "${BLUE}Starting console dev server (bun)...${NC}"
    cd console && bun run dev
}

run_setup() {
    echo -e "${BLUE}Fetching Rust dependencies...${NC}"
    (cd server && cargo fetch)
    echo -e "${BLUE}Installing console dependencies...${NC}"
    (cd console && bun install)
    echo -e "${GREEN}Setup complete${NC}"
}

run_clean() {
    echo -e "${YELLOW}Cleaning Rust target/...${NC}"
    (cd server && cargo clean)
    echo -e "${YELLOW}Cleaning console dist/ and .vite/...${NC}"
    rm -rf console/dist console/.vite
    echo -e "${GREEN}Clean complete${NC}"
}

ACTION="${1:-help}"
shift 2>/dev/null || true

case "$ACTION" in
    server|backend|rs) run_server ;;
    console|frontend|ui|ts) run_console ;;
    setup|install) run_setup ;;
    clean) run_clean ;;
    help|--help|-h) show_help ;;
    *) echo "Unknown: $ACTION. Run 'just dev' for help."; exit 1 ;;
esac
