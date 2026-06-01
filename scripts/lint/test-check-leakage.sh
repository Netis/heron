#!/usr/bin/env bash
# Self-test for check-leakage.sh.
#
# check-leakage.sh derives its repo root from its own location and scans
# `git ls-files`, so to exercise it we build a THROWAWAY git repo under
# $TMPDIR, drop a parallel scripts/lint/ (the script + its allowlist) plus
# whatever sample sources each case needs, and invoke the *copied* script
# there. The real working tree is never touched — a crash or SIGKILL can
# at worst leak a temp dir, never corrupt a tracked source file.
#
# Acceptance criteria covered:
#   1. A planted /home/<realuser>/ path in a source file is flagged.
#   2. Placeholder paths (~/..., /home/user/..., /Users/name/...) pass.
#   3. Captured fixtures are exempted via the file: allowlist patterns.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CHECK_SRC="$SCRIPT_DIR/check-leakage.sh"
ALLOW_SRC="$SCRIPT_DIR/leakage-allowlist.txt"

pass=0
fail=0
test_pass() { echo "✓ PASS: $1"; pass=$((pass + 1)); }
test_fail() { echo "✗ FAIL: $1"; fail=$((fail + 1)); }

# --- Hermetic sandbox: a real git repo, but disposable. ---
SANDBOX="$(mktemp -d)"
cleanup() { rm -rf "$SANDBOX"; }
trap cleanup EXIT

mkdir -p "$SANDBOX/scripts/lint"
cp "$CHECK_SRC" "$SANDBOX/scripts/lint/check-leakage.sh"
cp "$ALLOW_SRC" "$SANDBOX/scripts/lint/leakage-allowlist.txt"

git -C "$SANDBOX" init -q
git -C "$SANDBOX" config user.email test@example.com
git -C "$SANDBOX" config user.name test-check-leakage

CHECK="$SANDBOX/scripts/lint/check-leakage.sh"

# Stage everything (so git ls-files sees it) then run the copied linter.
run_check() {
  git -C "$SANDBOX" add -A
  bash "$CHECK" 2>&1 || true
}

# --- Test 1: real home path in a source file should be flagged ---
echo "Test 1: real home path in a source file should fail"
mkdir -p "$SANDBOX/server/h-common/src"
cat > "$SANDBOX/server/h-common/src/config.rs" <<'EOF'
//! sample source
/// Path: /home/somebody/secret.txt
pub fn test() {}
EOF
if run_check | grep -q "server/h-common/src/config.rs.*machine-specific home-directory path"; then
  test_pass "real home path detected"
else
  test_fail "real home path NOT detected"
fi

# --- Test 2: placeholder paths should pass ---
echo "Test 2: placeholder paths should pass"
cat > "$SANDBOX/server/h-common/src/config.rs" <<'EOF'
//! sample source
/// Home: ~/Downloads
/// Config: /home/user/.config
/// Docs: /Users/name/Documents
pub fn test() {}
EOF
if run_check | grep -q "config.rs.*machine-specific"; then
  test_fail "placeholder paths incorrectly flagged"
else
  test_pass "placeholder paths not flagged"
fi

# --- Test 3: captured fixtures should pass via the file: allowlist ---
# Reset the source to clean so only the fixture is under test, then plant a
# real home path inside an allow-listed fixture directory and confirm the
# allowlist (NOT the binary-file skip) is what exempts it.
echo "Test 3: fixture paths should pass via allowlist"
cat > "$SANDBOX/server/h-common/src/config.rs" <<'EOF'
pub fn test() {}
EOF
mkdir -p "$SANDBOX/server/h-protocol/tests/fixtures"
cat > "$SANDBOX/server/h-protocol/tests/fixtures/capture.txt" <<'EOF'
recorded wire path: /home/realuser/data.pcap
EOF
if run_check | grep -q "fixtures/capture.txt.*machine-specific"; then
  test_fail "fixture path incorrectly flagged (allowlist not working)"
else
  test_pass "fixture path exempt via allowlist"
fi

# --- Summary ---
echo ""
echo "========================================"
echo "Tests passed: $pass"
echo "Tests failed: $fail"
echo "========================================"

if [ "$fail" -gt 0 ]; then
  exit 1
fi

echo "All tests passed!"
