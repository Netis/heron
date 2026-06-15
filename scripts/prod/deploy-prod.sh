#!/usr/bin/env bash
# Deploy heron to PRODUCTION and gate on health, rolling back on failure.
#
# Runs on the `prod-deploy` self-hosted runner ON the prod host, so the deploy
# is LOCAL (build + systemctl restart) — no SSH/VM hop. It is pinned to a
# specific commit (the one that passed staging-soak) so prod gets exactly the
# validated source. The release binary is built in an Ubuntu-22.04 container
# (glibc-2.35-correct, with the BPF toolchain) so the prod host stays clean and
# the on-host eBPF capture engine is compiled in.
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
#   BUN_BIN              bun path                 (default: bun on PATH)
#
# The release binary is built in an Ubuntu-22.04 container (docker required) so
# the prod host carries no BPF toolchain; the binary ships the on-host eBPF
# SSL-uprobe capture engine. deploy ALSO ensures the unit's uprobe caps and an
# `ebpf` source in the config (idempotent), so a fresh host is fully set up.
#
# Exit: 0 = deployed + healthy; non-zero = failed (rolled back if possible).
set -euo pipefail

# Resolve the script's own dir BEFORE any cd. The Dockerfile must come from HERE
# (the runner workspace, checked out at the workflow ref = latest main, which
# carries it) — NOT from $REPO, which gets `git checkout $SHA`'d to the deploy
# target and may predate this file.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SHA="${1:-origin/main}"
REPO="${HERON_PROD_REPO_DIR:?set HERON_PROD_REPO_DIR (persistent heron checkout on the prod host)}"
SERVICE="${HERON_PROD_SERVICE:-heron.service}"
PORT="${HERON_PROD_PORT:-4500}"
HEALTH_TIMEOUT_SECS="${HEALTH_TIMEOUT_SECS:-120}"
# bun: explicit override → PATH → bun's canonical install dir. A non-login
# deploy shell often lacks ~/.bun/bin on PATH (same reason CARGO defaults to
# ~/.cargo/bin above), so fall back to the official-installer location rather
# than failing on a host where bun is installed but just not on PATH.
BUN="${BUN_BIN:-$(command -v bun 2>/dev/null || true)}"
[ -n "$BUN" ] || BUN="$HOME/.bun/bin/bun"

[ -d "$REPO/.git" ] || { echo "::error::HERON_PROD_REPO_DIR not a git checkout: $REPO" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || { echo "::error::docker not found — the release build runs in an Ubuntu-22.04 container so the host needs no BPF toolchain (see scripts/prod/Dockerfile.ebpf-build)" >&2; exit 1; }
[ -x "$BUN" ] || { echo "::error::bun not executable at '$BUN' — cannot rebuild the console bundle; set BUN_BIN or install bun; refusing to ship a stale embedded UI" >&2; exit 1; }
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

# Rebuild the console bundle FIRST. `--features console` embeds console/dist at
# COMPILE TIME via rust-embed (main.rs `#[folder = "../../../console/dist/"]`),
# and console/dist is gitignored — so `git checkout` never updates it. Without
# this step the cargo build below re-embeds whatever stale dist happened to be
# on the prod host from a prior manual build, and front-end changes (themes,
# pages) silently never reach prod even though the deploy reports success and
# the health gate (process-up + capture-running, not a UI check) stays green.
# Mirrors `just build all` (scripts/routers/shared/build.sh run_console).
echo "==> build console bundle (bun) — embedded into the binary via --features console"
( cd console && "$BUN" install && "$BUN" run build )
[ -n "$(ls -A "$REPO/console/dist" 2>/dev/null)" ] \
  || { echo "::error::console build produced no console/dist — refusing to embed an empty UI" >&2; exit 1; }
# rust-embed snapshots the folder at compile time and only re-embeds when the
# embedding crate recompiles; touch its source so an incremental cargo build
# actually picks up the freshly-built bundle instead of cached embedded bytes.
touch server/app/heron/src/main.rs

echo "==> build (release + console + eBPF) in an Ubuntu-22.04 container"
# Build inside a throwaway ubuntu:22.04 container, NOT on the host: the prod box
# then needs no BPF toolchain (nightly + rust-src + bpf-linker), and the binary
# links glibc 2.35 (the prod userland — building on a newer glibc would yield
# `GLIBC_2.xx not found` at runtime). The on-host SSL-uprobe capture engine is
# compiled in via `h-capture/ebpf` (the dependency feature is referenced
# directly, so this works whether or not the heron app exposes its own `ebpf`
# forwarding feature). A plain `--features console` host build would ship a
# binary that CANNOT run the `ebpf` source already in the prod config and would
# fail to start. See scripts/prod/Dockerfile.ebpf-build.
IMG="heron-ebpf-builder:22.04"
OUTDIR="$REPO/server/target/ebpf-out"
mkdir -p "$OUTDIR"
docker build -t "$IMG" -f "$SCRIPT_DIR/Dockerfile.ebpf-build" "$SCRIPT_DIR"
# Raise the container fd limit: parallel rustc across this host's many cores
# exhausts the default soft nofile (1024) → `cargo build` dies with "Too many
# open files (os error 24)". Cap at the host hard limit.
docker run --rm \
  --ulimit nofile=1048576:1048576 \
  -v "$REPO":/src:ro \
  -v "$OUTDIR":/out \
  "$IMG" bash -euo pipefail -c '
    git config --global --add safe.directory "*"
    mkdir -p /build/heron
    # Export the tracked tree at HEAD (= the pinned, soaked commit) via archive,
    # NOT clone (--local hardlinks fail across the mount/overlay device). Graft
    # in the host-built console bundle (gitignored, so not in the archive).
    git -C /src archive HEAD | tar -x -C /build/heron
    cp -r /src/console/dist /build/heron/console/dist
    cd /build/heron/server
    cargo build --release --bin heron --features "console h-capture/ebpf"
    install -m0755 target/release/heron /out/heron
  '
[ -x "$OUTDIR/heron" ] || { echo "::error::container build produced no binary" >&2; exit 1; }
install -m0755 "$OUTDIR/heron" "$BIN"

echo "==> smoke: heron --version"
"$BIN" --version || { echo "::error::freshly built binary does not run" >&2; exit 1; }

echo "==> ensure on-host eBPF prerequisites (idempotent)"
UNIT_PATH="/etc/systemd/system/$SERVICE"
unit_changed=0
if [ -f "$UNIT_PATH" ]; then
  # uprobe attach needs these caps. On kernels older than ~5.16 with a hardened
  # perf_event_paranoid, the uprobe perf_event_open is gated on CAP_SYS_ADMIN —
  # CAP_BPF + CAP_PERFMON alone is rejected (verified on the 5.15 prod host).
  for cap in CAP_BPF CAP_PERFMON CAP_SYS_ADMIN; do
    if grep -q "AmbientCapabilities=" "$UNIT_PATH" && ! grep -qE "AmbientCapabilities=.*\b$cap\b" "$UNIT_PATH"; then
      sudo sed -i -E "s/^(AmbientCapabilities=.*)$/\1 $cap/" "$UNIT_PATH"
      sudo sed -i -E "s/^(CapabilityBoundingSet=.*)$/\1 $cap/" "$UNIT_PATH"
      unit_changed=1
    fi
  done
  # Ensure the config the unit runs with (parsed from its `-c` flag) has an ebpf
  # source. heron's own startup is the authoritative TOML validator; the health
  # gate below rolls back if the edit is malformed.
  CONF_PATH="$(grep -oE -- '-c[= ]+[^ ]+' "$UNIT_PATH" | head -1 | sed -E 's/^-c[= ]+//')"
  if [ -n "$CONF_PATH" ] && [ -f "$CONF_PATH" ] && ! grep -q 'type = "ebpf"' "$CONF_PATH"; then
    python3 - "$CONF_PATH" <<'PY'
import sys
p = sys.argv[1]
lines = open(p).read().splitlines(keepends=True)
block = ("\n# eBPF SSL-uprobe source (added by deploy-prod). Empty ssl_libs =\n"
         "# autodetect libssl. Needs CAP_BPF+CAP_PERFMON+CAP_SYS_ADMIN (see unit).\n"
         '[[pipeline.sources]]\ntype = "ebpf"\nsource_id = "ebpf"\n\n')
out, done = [], False
for ln in lines:
    if not done and ln.lstrip().startswith("[storage]"):
        out.append(block); done = True
    out.append(ln)
if not done:
    out.append(block)
open(p, "w").write("".join(out))
print("  added ebpf source to", p)
PY
  fi
fi
if [ "$unit_changed" = 1 ]; then echo "  unit caps updated → daemon-reload"; sudo systemctl daemon-reload; fi

echo "==> restarting $SERVICE"
sudo systemctl restart "$SERVICE"

echo "==> health gate (<= ${HEALTH_TIMEOUT_SECS}s): status=ready AND pipeline running"
deadline=$(( $(date +%s) + HEALTH_TIMEOUT_SECS ))
ok=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  # status must be ready, and IF a capture pipeline is configured it must be
  # running. Empty `pipelines` (API-only / maintenance) → treat as healthy
  # (the process is up; there is nothing to supervise) rather than IndexError-
  # crashing into a false rollback of a healthy deploy.
  res="$(curl -s -m 5 "http://127.0.0.1:${PORT}/api/health" 2>/dev/null \
        | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin)["data"]; pl=d.get("pipelines") or []
    print(d["status"]+"|"+str(pl[0]["running"] if pl else True).lower())
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
