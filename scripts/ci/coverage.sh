#!/usr/bin/env bash
# Coverage driver for Heron — see docs/design/11-verification.md.
#
#   scripts/ci/coverage.sh rust            instrument + run the Rust workspace
#   scripts/ci/coverage.sh ts              instrument + run the console tests
#   scripts/ci/coverage.sh report          enforce floors / no-decrease (needs rust)
#   scripts/ci/coverage.sh diff [base]     enforce changed-code coverage (needs rust)
#   scripts/ci/coverage.sh all [base]      rust + report + diff  (the PR gate)
#
# Artifacts land in server/target/coverage/ and console/coverage/.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

COV_DIR="server/target/coverage"
LLVM_JSON="$COV_DIR/llvm-cov.json"
LCOV="$COV_DIR/lcov.info"
MODE="${1:-help}"
shift 2>/dev/null || true

run_rust() {
    mkdir -p "$COV_DIR"
    echo "[coverage] cargo llvm-cov --workspace (instrumented; this is the slow step)"
    (cd server && cargo llvm-cov --workspace --json --output-path "target/coverage/llvm-cov.json")
    (cd server && cargo llvm-cov report --lcov --output-path "target/coverage/lcov.info")
    echo "[coverage] wrote $LLVM_JSON and $LCOV"
}

run_ts() {
    echo "[coverage] bun test --coverage"
    (cd console && bun test --coverage --coverage-reporter=lcov --coverage-reporter=text)
    # Normalize SF: paths to repo-root-relative so diff_coverage.py can map them
    # to the console module regardless of whether bun emitted absolute or
    # console-relative paths.
    python3 - <<'PY'
import os
p = "console/coverage/lcov.info"
if os.path.exists(p):
    out = []
    for line in open(p):
        if line.startswith("SF:"):
            f = line[3:].strip()
            if not f.startswith("/") and not f.startswith("console/"):
                line = "SF:console/" + f + "\n"
        out.append(line)
    open(p, "w").writelines(out)
PY
    echo "[coverage] wrote console/coverage/lcov.info"
}

run_report() {
    [ -f "$LLVM_JSON" ] || { echo "missing $LLVM_JSON — run 'coverage.sh rust' first" >&2; exit 2; }
    python3 scripts/ci/check_coverage_policy.py --report "$LLVM_JSON" --repo-root .
}

run_diff() {
    local base="${1:-origin/main}"
    [ -f "$LCOV" ] || { echo "missing $LCOV — run 'coverage.sh rust' first" >&2; exit 2; }
    python3 scripts/ci/diff_coverage.py \
        --lcov "$LCOV" --cov-json "$LLVM_JSON" --base "$base" --repo-root .
}

case "$MODE" in
    rust)   run_rust ;;
    ts)     run_ts ;;
    report) run_report ;;
    diff)   run_diff "$@" ;;
    all)    run_rust; run_report; run_diff "$@" ;;
    help|--help|-h)
        sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
        ;;
    *) echo "unknown mode: $MODE (rust|ts|report|diff|all)" >&2; exit 2 ;;
esac
