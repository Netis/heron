#!/usr/bin/env bash
# Run the tara soak against the heron binary currently deployed on the staging
# VM. Resolves the VM via libvirt DHCP leases, ships tara + the corpus, runs
# the soak inside the VM, prints the verdict, and maps tara's exit to a job
# result. Mirrors deploy-staging.sh's VM-resolution + ssh model exactly.
#
# Usage:
#   scripts/staging/soak-staging.sh [corpus.pcap]
#
# Known-good promotion (makes the dual-binary self-test a STANDING gate):
# the baseline is a rolling "last binary that passed soak", stored on the VM
# at $HERON_STAGE_LASTGOOD. Each run soaks the freshly-deployed binary against
# it; on a pass the deployed binary is promoted to become the next known-good.
# So every new build is compared against the previous good build, and the
# known-good pointer advances on its own — no stale, hand-pinned baseline.
#   - first ever run (no known-good): candidate-only bootstrap, then promote.
#   - candidate regresses (exit 1): job fails, known-good UNCHANGED.
#   - baseline itself fails (harness_broken, exit 3): env/corpus problem or the
#     known-good needs re-baselining → warn, don't fail, don't advance.
#
# Env (same family as deploy-staging.sh):
#   HERON_STAGE_VM    REQUIRED  libvirt domain name (CI: repo Variable
#                               vars.HERON_STAGE_VM — never hardcoded, per the
#                               PR-hygiene/no-infra-in-source rule)
#   HERON_STAGE_USER  REQUIRED  ssh login on the VM (CI: vars.HERON_STAGE_USER)
#   HERON_STAGE_NET/SSH_KEY     libvirt net + ssh key (generic defaults:
#                               default / ~/.ssh/id_ed25519)
#   HERON_STAGE_BIN        binary on the VM to soak    (default /opt/heron/heron)
#   HERON_STAGE_LASTGOOD   rolling known-good path on the VM
#                                          (default /opt/heron/heron.last-good)
#   HERON_STAGE_BASELINE   pin an explicit baseline, overriding the rolling
#                          known-good (manual/debug; unset → use known-good)
#   HERON_STAGE_NO_PROMOTE set to 1 to soak without advancing the known-good
#   HERON_STAGE_LOAD_SECS  load-soak window seconds        (default 45)
#   HERON_STAGE_LOAD_PPS   load-soak steady rate (pkts/s)  (default 500)
#   HERON_STAGE_LOAD_ENFORCE  1 → a load-soak regression fails the deploy
#                          (default 0 = informational until thresholds are
#                          calibrated on this VM's CPU; the dual-binary baseline
#                          still flags relative regressions meanwhile)
#
# After the correctness soak passes, a deeper sustained-LOAD soak runs (steady
# rate_pps replay; perf + reliability invariants). It is informational by
# default — see HERON_STAGE_LOAD_ENFORCE.
#
# Exit: 0 pass · 1 candidate regressed · 2 setup error. A tara `harness_broken`
# (baseline itself failed → corpus/env problem, never the candidate) is logged
# as a warning and does NOT fail the job.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CORPUS="${1:-$HERE/../../server/h-protocol/tests/fixtures/keepalive_2sse_pipelined.pcap}"
[ -f "$CORPUS" ] || { echo "::error::corpus not found: $CORPUS" >&2; exit 2; }

# VM name + ssh login describe internal topology → never hardcode them in
# source; CI supplies them from repo Variables (see deploy-staging.yml).
VM="${HERON_STAGE_VM:?set HERON_STAGE_VM (libvirt domain; CI passes vars.HERON_STAGE_VM)}"
SSH_USER="${HERON_STAGE_USER:?set HERON_STAGE_USER (VM ssh login; CI passes vars.HERON_STAGE_USER)}"
NET="${HERON_STAGE_NET:-default}"
SSH_KEY="${HERON_STAGE_SSH_KEY:-$HOME/.ssh/id_ed25519}"
STAGE_BIN="${HERON_STAGE_BIN:-/opt/heron/heron}"
LAST_GOOD="${HERON_STAGE_LASTGOOD:-/opt/heron/heron.last-good}"
BASELINE="${HERON_STAGE_BASELINE:-}"
PROMOTE=1; [ "${HERON_STAGE_NO_PROMOTE:-0}" = 1 ] && PROMOTE=0

SSH_OPTS=(-i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=8)
remote() { ssh "${SSH_OPTS[@]}" "$SSH_USER@$IP" "$@"; }

echo "==> resolving $VM IP from libvirt '$NET' DHCP leases"
IP="$(sudo virsh net-dhcp-leases "$NET" 2>/dev/null | awk -v n="$VM" '$0 ~ n {print $5}' | cut -d/ -f1 | head -1)"
[ -n "$IP" ] || { echo "::error::no DHCP lease for domain '$VM' on net '$NET' — is the VM running?" >&2; exit 2; }
echo "    $VM -> $IP  (soak binary: $STAGE_BIN)"

RD="/tmp/tara.$$"
remote "mkdir -p $RD"
# shellcheck disable=SC2064
trap "ssh ${SSH_OPTS[*]} $SSH_USER@$IP 'rm -rf $RD' >/dev/null 2>&1 || true" EXIT
scp "${SSH_OPTS[@]}" "$HERE/tara.sh" "$HERE/tara_invariants.py" "$CORPUS" "$SSH_USER@$IP:$RD/"
CORPUS_VM="$RD/$(basename "$CORPUS")"

# Resolve the baseline: an explicit pin wins, else the rolling known-good if
# one exists on the VM, else none (bootstrap — the very first soak).
if [ -z "$BASELINE" ] && remote "test -x '$LAST_GOOD'" 2>/dev/null; then
  BASELINE="$LAST_GOOD"
fi
if [ -n "$BASELINE" ]; then
  echo "    self-test baseline: $BASELINE"
  base_arg="--baseline '$BASELINE'"
else
  echo "    no known-good baseline yet → candidate-only bootstrap soak"
  base_arg=""
fi

echo "==> running tara soak inside the VM"
set +e
remote "cd $RD && bash tara.sh --binary '$STAGE_BIN' --corpus '$CORPUS_VM' $base_arg --json-out $RD/out.json"
rc=$?
set -e

echo "==> verdict:"
remote "cat $RD/out.json 2>/dev/null" || echo "(no verdict file — tara aborted before writing)"

case "$rc" in
  0)
    echo "==> SOAK PASS — staging binary healthy under replay"

    # Deeper check: a sustained-load soak (steady rate_pps replay for a window),
    # asserting perf + reliability invariants — queue depth bounded, zero
    # backpressure drops, RSS growth bounded, no flush errors. Runs only after
    # the correctness soak is green. INFORMATIONAL by default: the absolute
    # queue/RSS thresholds still need one calibration pass on this VM's CPU, so
    # a load regression is logged + warned but does NOT fail the deploy yet (the
    # dual-binary baseline still flags a *relative* regression). Flip to a hard
    # gate with HERON_STAGE_LOAD_ENFORCE=1 once calibrated.
    echo "==> running tara LOAD soak inside the VM (informational)"
    set +e
    remote "cd $RD && bash tara.sh --binary '$STAGE_BIN' --corpus '$CORPUS_VM' $base_arg --load --duration ${HERON_STAGE_LOAD_SECS:-45} --rate-pps ${HERON_STAGE_LOAD_PPS:-500} --json-out $RD/load.json"
    lrc=$?
    set -e
    echo "==> load verdict:"
    remote "cat $RD/load.json 2>/dev/null" || echo "(no load verdict file)"
    if [ "$lrc" != 0 ] && [ "$lrc" != 3 ]; then
      if [ "${HERON_STAGE_LOAD_ENFORCE:-0}" = 1 ]; then
        echo "::error::LOAD SOAK FAILED (tara rc=$lrc) and HERON_STAGE_LOAD_ENFORCE=1 — failing deploy; known-good UNCHANGED"
        exit 1
      fi
      echo "::warning::load soak reported a regression (tara rc=$lrc) — INFORMATIONAL only (set HERON_STAGE_LOAD_ENFORCE=1 to gate). Promotion below uses the correctness soak."
    fi

    if [ "$PROMOTE" = 1 ]; then
      if remote "sudo install -m 0755 '$STAGE_BIN' '$LAST_GOOD'"; then
        echo "==> known-good advanced → $LAST_GOOD (next soak compares against this build)"
      else
        echo "::warning::soak passed but could not advance known-good ($LAST_GOOD)"
      fi
    fi
    exit 0;;
  3)
    echo "::warning::tara harness_broken — the baseline (known-good) itself failed the soak, so this is a corpus/env problem or the known-good needs re-baselining, NOT a candidate regression. Not failing the job; known-good UNCHANGED."
    exit 0;;
  *)
    echo "::error::SOAK FAILED — the deployed staging binary regressed (tara rc=$rc); known-good UNCHANGED"
    exit 1;;
esac
