#!/usr/bin/env bash
set -euo pipefail

# Colors
CYAN='\033[0;36m'
DIM='\033[2m'
BOLD='\033[1m'
NC='\033[0m'

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$PROJECT_ROOT" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
fi
cd "$PROJECT_ROOT"

# Check tokei is installed
if ! command -v tokei &>/dev/null; then
    echo "Error: tokei not found. Install with: brew install tokei"
    exit 1
fi

show_help() {
    echo ""
    echo "📊 Lines of Code"
    echo "   just loc all          Full dashboard"
    echo "   just loc server       Rust backend only"
    echo "   just loc console      React frontend only"
    echo "   just loc docs         Design docs only"
    echo "   just loc scripts      Scripts only"
}

# Extract Code column (field $4) from tokei Total row
# Usage: count_code <path> [exclude...]
count_code() {
    local path="$1"; shift
    local args=("$path")
    for ex in "$@"; do args+=(--exclude "$ex"); done
    tokei "${args[@]}" 2>/dev/null | grep '^ Total' | awk '{print $4}'
}

# Extract Lines column (field $3) — better for markdown-heavy dirs
# Usage: count_lines <path> [exclude...]
count_lines() {
    local path="$1"; shift
    local args=("$path")
    for ex in "$@"; do args+=(--exclude "$ex"); done
    tokei "${args[@]}" 2>/dev/null | grep '^ Total' | awk '{print $3}'
}

# Run tokei for a section and print its table
run_section() {
    local path="$1"; shift
    tokei "$path" "$@" 2>/dev/null
}

# ── Section runners ──────────────────────────────────────────────────

run_server() {
    echo -e "${BOLD}${CYAN}  Server${NC} ${DIM}(Rust backend)${NC}"
    echo -e "  ${DIM}Excludes: target/${NC}"
    echo ""
    run_section server --exclude server/target
}

run_console() {
    echo -e "${BOLD}${CYAN}  Console${NC} ${DIM}(React frontend)${NC}"
    echo -e "  ${DIM}Excludes: node_modules/, dist/, components/ui/${NC}"
    echo ""
    run_section console --exclude console/node_modules --exclude console/dist --exclude 'console/src/components/ui'
}

run_docs() {
    echo -e "${BOLD}${CYAN}  Docs${NC} ${DIM}(design documents)${NC}"
    echo ""
    run_section docs
}

run_scripts() {
    echo -e "${BOLD}${CYAN}  Scripts${NC} ${DIM}(dev/build helpers)${NC}"
    echo ""
    run_section scripts
}

# ── Dashboard ────────────────────────────────────────────────────────

run_dashboard() {
    # Source dirs → Code column
    local server_loc console_loc scripts_loc
    server_loc=$(count_code server server/target)
    console_loc=$(count_code console console/node_modules console/dist 'console/src/components/ui')
    scripts_loc=$(count_code scripts)

    # Markdown-heavy dirs → Lines column
    local docs_loc claude_loc
    docs_loc=$(count_lines docs)
    claude_loc=$(count_lines .claude)

    # Fallback to 0 if empty
    server_loc=${server_loc:-0}
    console_loc=${console_loc:-0}
    scripts_loc=${scripts_loc:-0}
    docs_loc=${docs_loc:-0}
    claude_loc=${claude_loc:-0}

    local source_total=$((server_loc + console_loc))
    local grand_total=$((source_total + scripts_loc + docs_loc + claude_loc))

    echo ""
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}  📊 CODEBASE — Lines of Code Dashboard${NC}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
    echo -e "  ${BOLD}Source Code${NC}  ${DIM}(Code column)${NC}"
    printf "    %-22s %s\n" "Server (Rust)" "$(printf '%6s' "$server_loc") LOC"
    printf "    %-22s %s\n" "Console (TS/React)" "$(printf '%6s' "$console_loc") LOC"
    echo -e "    ${DIM}──────────────────────────────────────${NC}"
    printf "    ${BOLD}%-22s %s${NC}\n" "Subtotal" "$(printf '%6s' "$source_total") LOC"
    echo ""
    echo -e "  ${BOLD}Infrastructure${NC}  ${DIM}(Code column)${NC}"
    printf "    %-22s %s\n" "Scripts" "$(printf '%6s' "$scripts_loc") LOC"
    echo ""
    echo -e "  ${BOLD}Content${NC}  ${DIM}(Lines column — markdown)${NC}"
    printf "    %-22s %s\n" "Docs (design)" "$(printf '%6s' "$docs_loc") lines"
    echo ""
    echo -e "  ${BOLD}AI Config${NC}  ${DIM}(Lines column — markdown)${NC}"
    printf "    %-22s %s\n" ".claude" "$(printf '%6s' "$claude_loc") lines"
    echo ""
    echo -e "  ${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    printf "  ${BOLD}%-24s %s${NC}\n" "Grand Total" "$(printf '%6s' "$grand_total")"
    echo ""
    echo -e "  ${DIM}Excludes: target/, node_modules/, dist/, components/ui/${NC}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
}

# ── Dispatch ─────────────────────────────────────────────────────────

ACTION="${1:-help}"
shift 2>/dev/null || true

case "$ACTION" in
    server|backend|rs)       run_server ;;
    console|frontend|ui|ts)  run_console ;;
    docs|doc)                run_docs ;;
    scripts)                 run_scripts ;;
    all|dashboard)           run_dashboard ;;
    help|--help|-h)          show_help ;;
    *) echo "Unknown: $ACTION. Run 'just loc' for help."; exit 1 ;;
esac
