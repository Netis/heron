#!/usr/bin/env bash
# Validate that the Olympus migration is complete and internally consistent.
#
# The expected Olympus version is NOT hardcoded here — it is derived from the
# first wrapper's `olympus_ref:` and treated as the source of truth, so a future
# version bump needs no edit to this script. A bump is enforced by asserting
# every wrapper agrees on that version (the class of bug that left pr-review.yml
# ahead of the other four).
#
# Checks:
#   1. .olympus.json exists, .agent-ops.json does not
#   2. .olympus.json $schema points to Netis/olympus
#   3. All 5 wrapper workflows use olympus_ref (not agent_ops_ref)
#   4. All 5 wrappers agree on one olympus_ref version, and each wrapper's
#      `uses: Netis/olympus/...@vX.Y.Z` pin matches its own olympus_ref
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

fail=0
fail_one() { echo "  FAIL: $1"; fail=$((fail + 1)); }
pass_one() { echo "  PASS: $1"; }

WRAPPERS=(
  ".github/workflows/guard.yml"
  ".github/workflows/issue-implement.yml"
  ".github/workflows/issue-triage.yml"
  ".github/workflows/pr-revise.yml"
  ".github/workflows/pr-review.yml"
)

echo "=== Olympus migration check ==="
echo ""

# 1. Policy file
echo "--- Policy file ---"
if [ -f .olympus.json ]; then
  pass_one ".olympus.json exists"
else
  fail_one ".olympus.json missing"
fi

if [ ! -f .agent-ops.json ]; then
  pass_one ".agent-ops.json removed"
else
  fail_one ".agent-ops.json still present"
fi

if grep -q 'Netis/olympus/main/schema/olympus.schema.json' .olympus.json 2>/dev/null; then
  pass_one '$schema points to olympus.schema.json'
else
  fail_one '$schema does not point to olympus.schema.json'
fi

# 2. Workflow wrappers
echo ""
echo "--- Workflow wrappers ---"

# Extract the `olympus_ref:` version (e.g. v0.4.0) from a wrapper, or "" if none.
ref_version() {
  grep -oE 'olympus_ref:[[:space:]]*v[0-9]+\.[0-9]+\.[0-9]+' "$1" \
    | head -1 | grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+'
}

EXPECTED="$(ref_version "${WRAPPERS[0]}")"
if [ -n "$EXPECTED" ]; then
  pass_one "expected olympus version derived from $(basename "${WRAPPERS[0]}"): $EXPECTED"
else
  fail_one "could not derive olympus_ref version from $(basename "${WRAPPERS[0]}")"
fi

for wf in "${WRAPPERS[@]}"; do
  base="$(basename "$wf")"
  if ! grep -q 'agent_ops_ref' "$wf"; then
    pass_one "$base: no agent_ops_ref"
  else
    fail_one "$base: still uses agent_ops_ref"
  fi

  wf_ref="$(ref_version "$wf")"
  if [ -n "$wf_ref" ] && [ "$wf_ref" = "$EXPECTED" ]; then
    pass_one "$base: olympus_ref $wf_ref (agrees)"
  else
    fail_one "$base: olympus_ref '${wf_ref:-<none>}' != expected '$EXPECTED'"
  fi

  # The `uses:` pin must match this wrapper's own olympus_ref version.
  if [ -n "$wf_ref" ] && grep -E 'uses:.*Netis/olympus' "$wf" | grep -qF "@$wf_ref"; then
    pass_one "$base: uses-pin matches @$wf_ref"
  else
    fail_one "$base: uses-pin does not match olympus_ref '${wf_ref:-<none>}'"
  fi
done

echo ""
echo "========================================"
echo "Checks failed: $fail"
echo "========================================"

if [ "$fail" -gt 0 ]; then
  exit 1
fi
echo "Olympus ${EXPECTED} migration validated."
