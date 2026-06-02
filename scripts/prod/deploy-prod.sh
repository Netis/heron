#!/usr/bin/env bash
# Deploy heron to PRODUCTION and gate on health, rolling back on failure.
#
# Runs on the `prod-deploy` self-hosted runner ON the prod host, so the deploy
# is LOCAL (build + systemctl restart) — no SSH/VM hop. It is pinned to a
# specific commit (the one that passed staging-soak) so prod gets exactly the
# validated source, and it builds in the PERSISTENT checkout (warm cargo cache
# → incremental, fast) rather than a fresh runner workspace.
#
# Safety:
#   - builds BEFORE touching the running service, so a build failure leaves
#     prod untouched;
#   - snapshots the current binary first and rolls back + restarts if the
#     post-restart health gate fails;
#   - heron.service grants capture caps via AmbientCapabilities, so no setcap.
#
# Usage:
#   deploy-prod.sh [<git-sha>]        (default: origin/main HEAD)
#
# Env:
#   HERON_PROD_REPO_DIR  REQUIRED  persistent heron checkout (warm cargo cache)
#   HERON_PROD_SERVICE   systemd unit             (default: heron.service)
#   HERON_PROD_PORT      heron API port           (default: 4500)
#   HEALTH_TIMEOUT_SECS  health-gate budget secs  (default: 120)
#   CARGO_BIN            cargo path               (default: ~/.cargo/bin/cargo)
#
# Exit: 0 = deployed + healthy; non-zero = failed (rolled back if possible).
set -euo pipefail

SHA="${1:-origin/main}"
REPO="${HERON_PROD_REPO_DIR:?set HERON_PROD_REPO_DIR (persistent heron checkout on the prod host)}"
SERVICE="${HERON_PROD_SERVICE:-heron.service}"
PORT="${HERON_PROD_PORT:-4500}"
HEALTH_TIMEOUT_SECS="${HEALTH_TIMEOUT_SECS:-120}"
CARGO="${CARGO_BIN:-$HOME/.cargo/bin/cargo}"

[ -d "$REPO/.git" ] || { echo "::error::HERON_PROD_REPO_DIR not a git checkout: $REPO" >&2; exit 1; }
[ -x "$CARGO" ] || { echo "::error::cargo not executable at $CARGO" >&2; exit 1; }
cd "$REPO"

BIN="$REPO/server/target/release/heron"
BAK="$BIN.rollback"

echo "==> fetch + checkout $SHA"
git fetch origin --quiet
git checkout --quiet "$SHA"
echo "    HEAD: $(git log --oneline -1)"

# Snapshot the currently-running binary BEFORE the build overwrites it in place.
if [ -x "$BIN" ]; then
  echo "==> snapshotting current binary → $(basename "$BAK")"
  cp -fp "$BIN" "$BAK"
  HAVE_BAK=1
else
  echo "    (no existing binary to back up — first deploy)"
  HAVE_BAK=0
fi

echo "==> build (release + console, incremental — MUST pass --features console)"
( cd server && "$CARGO" build --release --bin heron --features console )
[ -x "$BIN" ] || { echo "::error::build produced no binary at $BIN" >&2; exit 1; }

echo "==> smoke: heron --version"
"$BIN" --version || { echo "::error::freshly built binary does not run" >&2; exit 1; }

echo "==> restarting $SERVICE"
sudo systemctl restart "$SERVICE"

echo "==> health gate (<= ${HEALTH_TIMEOUT_SECS}s): status=ready AND pipeline running"
deadline=$(( $(date +%s) + HEALTH_TIMEOUT_SECS ))
ok=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  res="$(curl -s -m 5 "http://127.0.0.1:${PORT}/api/health" 2>/dev/null \
        | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin)["data"]; print(d["status"]+"|"+str(d["pipelines"][0]["running"]).lower())
except Exception: print("|")' 2>/dev/null || echo "|")"
  if [ "${res%%|*}" = "ready" ] && [ "${res##*|}" = "true" ]; then ok=1; break; fi
  sleep 5
done

if [ "$ok" = 1 ]; then
  echo "==> OK prod heron healthy on :${PORT} (status=ready, capturing)"
  rm -f "$BAK"
  exit 0
fi

echo "::error::health gate FAILED after ${HEALTH_TIMEOUT_SECS}s" >&2
if [ "$HAVE_BAK" = 1 ]; then
  echo "==> rolling back to the previous binary + restarting"
  cp -fp "$BAK" "$BIN"
  sudo systemctl restart "$SERVICE"
  sleep 5
  rb="$(curl -s -m 5 "http://127.0.0.1:${PORT}/api/health" 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["data"]["status"])
except Exception: print("?")' 2>/dev/null || echo "?")"
  echo "    rollback health: status=$rb"
  rm -f "$BAK"
else
  echo "::error::no rollback binary available (first deploy)" >&2
fi
exit 1
