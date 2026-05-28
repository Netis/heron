#!/usr/bin/env bash
# Rebrand verification test — ensures no old branding remains in scope.
# Exit 0 if clean, exit 1 if any legacy references found.
set -euo pipefail

# Operator scripts (Phase 4 scope)
SCRIPT_FILES=(
    scripts/demo-server-setup.sh
    scripts/lib/demo-common.sh
    scripts/reindex_turns.py
    scripts/traffic-gen.py
    scripts/lint/check-secrets.sh
)

# Rust doc comments (Phase 4 scope)
RUST_FILES=(
    server/ts-llm/src/wire_apis/mod.rs
    server/ts-llm/src/agents/generic.rs
    server/ts-turn/src/proxy_pair.rs
    server/ts-pcap-extract/src/filter.rs
)

# Adjust paths if Phase 2 (ts-* → h-*) has landed
for f in "${RUST_FILES[@]}"; do
    if [[ ! -f "$f" ]]; then
        # Try h-* variant
        h_f="${f/ts-/h-}"
        if [[ -f "$h_f" ]]; then
            RUST_FILES[${RUST_FILES[$i]}]="$h_f"
        fi
    fi
done

errors=0

echo "Checking operator scripts for legacy branding..."
for f in "${SCRIPT_FILES[@]}"; do
    if [[ -f "$f" ]]; then
        if grep -qiE 'tokenscope|TokenScope' "$f" 2>/dev/null; then
            echo "::error::$f contains legacy branding (tokenscope/TokenScope)"
            grep -niE 'tokenscope|TokenScope' "$f" || true
            ((errors++))
        fi
    else
        echo "::warning::$f not found (may have been moved or deleted)"
    fi
done

echo "Checking Rust doc comments for legacy branding..."
for f in "${RUST_FILES[@]}"; do
    if [[ -f "$f" ]]; then
        if grep -qiE 'tokenscope|TokenScope' "$f" 2>/dev/null; then
            echo "::error::$f contains legacy branding (tokenscope/TokenScope)"
            grep -niE 'tokenscope|TokenScope' "$f" || true
            ((errors++))
        fi
    else
        echo "::warning::$f not found (may have been moved or deleted)"
    fi
done

if [[ $errors -eq 0 ]]; then
    echo "check-rebrand: ✓ no legacy branding found in scope"
    exit 0
else
    echo "check-rebrand: $errors file(s) contain legacy branding"
    exit 1
fi