#!/usr/bin/env bash
# Mutation-testing driver — see docs/design/11-verification.md.
#
#   scripts/ci/mutation.sh diff [base]   mutate only the PR's changed Tier 0 lines
#   scripts/ci/mutation.sh full          full run over every Tier 0 crate
#
# Mutation proves the tests would notice a wrong implementation. It is scoped to
# Tier 0 because the bundled-DuckDB build makes it prohibitive elsewhere.
#
# Requires `cargo-mutants` (cargo install cargo-mutants --locked). The floor is
# read from verification/coverage-policy.json (tiers["0"].mutation).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MODE="${1:-help}"
shift 2>/dev/null || true

require_mutants() {
    command -v cargo-mutants >/dev/null 2>&1 || {
        echo "cargo-mutants not installed; run: cargo install cargo-mutants --locked" >&2
        exit 2
    }
}

TIER0=$(python3 - <<'PY'
import json
p = json.load(open("verification/coverage-policy.json"))
print(" ".join(name for name, m in p["modules"].items() if int(m["tier"]) == 0))
PY
)
FLOOR=$(python3 - <<'PY'
import json
print(json.load(open("verification/coverage-policy.json"))["tiers"]["0"]["mutation"])
PY
)

score() {
    python3 - "${1:-0}" <<'PY'
import json, sys
try:
    import os
    for cand in ("mutants.out/outcomes.json", "server/mutants.out/outcomes.json"):
        if os.path.exists(cand):
            outcomes = json.load(open(cand))["outcomes"]
            break
    else:
        raise FileNotFoundError("outcomes.json")
except (OSError, KeyError, ValueError):
    print("mutation: no outcomes.json; treat as failure", file=sys.stderr)
    sys.exit(1)
killed = sum(1 for o in outcomes if o.get("summary") == "CaughtMutant")
survived = sum(1 for o in outcomes if o.get("summary") == "MissedMutant")
caught = killed + survived
pct = 100.0 * killed / caught if caught else 100.0
floor = float(sys.argv[1])
print(f"mutation score: {pct:.1f}% ({killed}/{caught} caught, floor {floor}%)")
sys.exit(0 if pct + 1e-9 >= floor else 1)
PY
}

run_diff() {
    require_mutants
    local base="${1:-origin/main}"
    local patch; patch="$(mktemp)"; trap 'rm -f "$patch"' EXIT
    # Paths must be relative to the workspace root (`server/`), which is what
    # cargo-mutants matches against: `--relative` from inside server/ strips the
    # `server/` prefix. Repo-relative paths select zero mutants (silently).
    # `--in-place` is then mandatory anyway: cargo-mutants copies only the
    # `server/` workspace, but `h-common` does `include_str!("../../../VERSION")`
    # from the repo root, so a copy never builds. It cannot combine with
    # `--jobs`, so mutation runs single-threaded.
    # shellcheck disable=SC2086
    (cd server && git diff --unified=0 --relative "$base"...HEAD -- $TIER0) > "$patch" || true
    if [ ! -s "$patch" ]; then echo "mutation: no Rust diff vs $base"; exit 0; fi
    # shellcheck disable=SC2086
    (cd server && cargo mutants $(for c in $TIER0; do printf -- '-p %s ' "$c"; done) \
        --in-diff "$patch" --in-place --timeout 300 || true)
    score "$FLOOR"
}

run_full() {
    require_mutants
    local rc=0
    # Optional package list; default is every Tier 0 crate. The DuckDB-backed
    # crates are impractically slow to mutate in full (single-threaded, in-place,
    # one DuckDB link per mutant), so scheduled runs pass only the feasible set
    # and rely on `diff` mode for the DuckDB crates.
    local crates="${*:-$TIER0}"
    for c in $crates; do
        echo "==> mutation: $c"
        (cd server && cargo mutants -p "$c" --in-place --timeout 300 || true)
        score "$FLOOR" || rc=1
    done
    exit "$rc"
}

case "$MODE" in
    diff) run_diff "$@" ;;
    full) run_full "$@" ;;
    help|--help|-h) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//' ;;
    *) echo "unknown mode: $MODE (diff|full)" >&2; exit 2 ;;
esac
