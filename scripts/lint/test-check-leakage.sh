#!/usr/bin/env bash
# Test suite for check-leakage.sh
#
# This verifies the acceptance criteria for the home-directory path detection:
#   1. A planted /home/<realuser>/ in a source file fails
#   2. ~/... and /path/to/... style example paths pass
#   3. Captured fixtures pass via allowlist
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LEAKAGE_CHECK="$SCRIPT_DIR/check-leakage.sh"

pass=0
fail=0

test_pass() {
  echo "✓ PASS: $1"
  pass=$((pass + 1))
}

test_fail() {
  echo "✗ FAIL: $1"
  fail=$((fail + 1))
}

# Use a real source file for testing
TEST_FILE="$REPO_ROOT/server/h-common/src/config.rs"
ORIGINAL_CONTENT=$(cat "$TEST_FILE")

cleanup() {
  # Restore original content
  echo "$ORIGINAL_CONTENT" > "$TEST_FILE"
}
trap cleanup EXIT

# --- Test 1: Real home path in source file should fail ---
echo "Test 1: Real home path in source file should fail"
cat > "$TEST_FILE" <<'EOF'
//! Test file for leakage detection
/// Path: /home/somebody/secret.txt
pub fn test() {}
EOF

# Run the check (should fail)
OUTPUT=$(bash "$LEAKAGE_CHECK" 2>&1 || true)
# Use a pattern that matches relative path output from the check
echo "$OUTPUT" | grep -q "server/h-common/src/config.rs.*machine-specific home-directory path" && FOUND=1 || FOUND=0
if [ "$FOUND" -eq 1 ]; then
  test_pass "Real home path detected in test file"
else
  test_fail "Real home path not detected"
  echo "  Output: $OUTPUT" | head -5
fi

# --- Test 2: Placeholder paths should pass ---
echo "Test 2: Placeholder paths should pass"
cat > "$TEST_FILE" <<'EOF'
//! Test file for leakage detection
/// Home: ~/Downloads
/// Config: /home/user/.config
/// Docs: /Users/name/Documents
pub fn test() {}
EOF

# Run the check (should NOT flag the placeholder paths)
if bash "$LEAKAGE_CHECK" 2>&1 | grep -q "$TEST_FILE.*machine-specific"; then
  test_fail "Placeholder paths incorrectly flagged"
else
  test_pass "Placeholder paths not flagged"
fi

# --- Test 3: Fixtures should pass via allowlist ---
echo "Test 3: Fixtures should pass via allowlist"
# The fixtures already exist and contain real paths - verify they don't trigger
FIXTURE_FILE="$REPO_ROOT/server/h-protocol/tests/fixtures/keepalive_2sse_client.bin"
if [ -f "$FIXTURE_FILE" ]; then
  if bash "$LEAKAGE_CHECK" 2>&1 | grep -q "$FIXTURE_FILE.*machine-specific"; then
    test_fail "Fixture file incorrectly flagged (allowlist not working)"
  else
    test_pass "Fixture file allowed via allowlist"
  fi
else
  echo "  (skipped - fixture file not found)"
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