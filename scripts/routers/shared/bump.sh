#!/usr/bin/env bash
# Version bump router.
#
# SSOT: <repo-root>/VERSION (plain semver, trailing newline).
# Derived files kept in sync: server/Cargo.toml (workspace.package.version),
# console/package.json ("version").
#
# Rust code reads VERSION via ts_common::version::version() (compile-time
# include_str!). Frontend reads VERSION via Vite define __APP_VERSION__
# (build-time). package.json/Cargo.toml versions exist only because those
# toolchains require literal version fields — they are NOT sources of truth.
set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
BLUE='\033[0;34m'
NC='\033[0m'

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$PROJECT_ROOT" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
fi
cd "$PROJECT_ROOT"

VERSION_FILE="$PROJECT_ROOT/VERSION"
CARGO_FILE="$PROJECT_ROOT/server/Cargo.toml"
PKG_FILE="$PROJECT_ROOT/console/package.json"

show_help() {
    echo ""
    echo "⚡ Version Bump"
    echo "   just bump show          Print current VERSION + drift check"
    echo "   just bump check         Verify VERSION == Cargo.toml == package.json"
    echo "   just bump patch         x.y.z → x.y.(z+1)"
    echo "   just bump minor         x.y.z → x.(y+1).0"
    echo "   just bump major         x.y.z → (x+1).0.0"
    echo "   just bump set <X.Y.Z>   Set exact version"
    echo ""
    echo "VERSION (repo root) is the single source of truth."
    echo "Cargo.toml and package.json are derived — rewritten by this script."
}

read_version_file() {
    if [ ! -f "$VERSION_FILE" ]; then
        echo -e "${RED}ERROR:${NC} $VERSION_FILE does not exist" >&2
        exit 1
    fi
    tr -d '[:space:]' < "$VERSION_FILE"
}

read_cargo_version() {
    awk '/^\[workspace\.package\]/{f=1; next} f && /^version[[:space:]]*=/{gsub(/[" ]/,""); split($0,a,"="); print a[2]; exit}' "$CARGO_FILE"
}

read_pkg_version() {
    awk -F'"' '/^[[:space:]]*"version"[[:space:]]*:/{print $4; exit}' "$PKG_FILE"
}

check_drift() {
    local v_file v_cargo v_pkg
    v_file="$(read_version_file)"
    v_cargo="$(read_cargo_version)"
    v_pkg="$(read_pkg_version)"
    echo "  VERSION:              $v_file"
    echo "  server/Cargo.toml:    $v_cargo"
    echo "  console/package.json: $v_pkg"
    if [ "$v_file" = "$v_cargo" ] && [ "$v_file" = "$v_pkg" ]; then
        echo -e "${GREEN}✓ in sync${NC}"
        return 0
    else
        echo -e "${RED}✗ drift detected${NC}"
        return 1
    fi
}

validate_semver() {
    if ! [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        echo -e "${RED}ERROR:${NC} '$1' is not X.Y.Z semver" >&2
        exit 1
    fi
}

write_version_file() {
    printf '%s\n' "$1" > "$VERSION_FILE"
}

write_cargo_version() {
    # Replace the version line under [workspace.package].
    # Uses a state-aware awk so we only touch the workspace.package section.
    local new="$1" tmp
    tmp="$(mktemp)"
    awk -v new="$new" '
        /^\[/ { section = $0 }
        section == "[workspace.package]" && /^version[[:space:]]*=/ {
            print "version = \"" new "\""; next
        }
        { print }
    ' "$CARGO_FILE" > "$tmp"
    mv "$tmp" "$CARGO_FILE"
}

write_pkg_version() {
    local new="$1" tmp
    tmp="$(mktemp)"
    # Replace only the first top-level "version": "x.y.z" line.
    awk -v new="$new" '
        !done && /^[[:space:]]*"version"[[:space:]]*:/ {
            sub(/"[0-9]+\.[0-9]+\.[0-9]+"/, "\"" new "\"")
            done = 1
        }
        { print }
    ' "$PKG_FILE" > "$tmp"
    mv "$tmp" "$PKG_FILE"
}

sync_all() {
    local new="$1"
    write_version_file "$new"
    write_cargo_version "$new"
    write_pkg_version "$new"
    echo -e "${GREEN}✓${NC} VERSION → $new"
    echo -e "${GREEN}✓${NC} server/Cargo.toml → $new"
    echo -e "${GREEN}✓${NC} console/package.json → $new"
}

bump_component() {
    local current="$1" kind="$2"
    IFS='.' read -r major minor patch <<< "$current"
    case "$kind" in
        major) echo "$((major + 1)).0.0" ;;
        minor) echo "$major.$((minor + 1)).0" ;;
        patch) echo "$major.$minor.$((patch + 1))" ;;
        *) echo "$current" ;;
    esac
}

cmd_show() {
    echo -e "${BLUE}Version status${NC}"
    check_drift || true
}

cmd_check() {
    echo -e "${BLUE}Version sync check${NC}"
    check_drift
}

cmd_bump() {
    local kind="$1" current new
    current="$(read_version_file)"
    validate_semver "$current"
    new="$(bump_component "$current" "$kind")"
    echo -e "${YELLOW}bump $kind:${NC} $current → $new"
    sync_all "$new"
    echo ""
    echo "Next steps (manual):"
    echo "  - Update CHANGELOG.md"
    echo "  - git commit -am 'bump: v$new' && git tag v$new"
}

cmd_set() {
    local new="${1:-}"
    validate_semver "$new"
    local current
    current="$(read_version_file)"
    echo -e "${YELLOW}set:${NC} $current → $new"
    sync_all "$new"
}

action="${1:-help}"; shift || true
case "$action" in
    help|-h|--help|"") show_help ;;
    show)              cmd_show ;;
    check)             cmd_check ;;
    patch|minor|major) cmd_bump "$action" ;;
    set)               cmd_set "${1:-}" ;;
    *)
        echo -e "${RED}Unknown bump action: $action${NC}"
        show_help
        exit 1
        ;;
esac
