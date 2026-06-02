#!/usr/bin/env bash
# Run the tara soak against the heron binary currently deployed on the staging
# VM. Resolves the VM via libvirt DHCP leases, ships tara + the corpus, runs
# the soak inside the VM, prints the verdict, and maps tara's exit to a job
# result. Mirrors deploy-staging.sh's VM-resolution + ssh model exactly.
#
# Usage:
#   scripts/staging/soak-staging.sh [corpus.pcap]
#
# Env (all optional; same family as deploy-staging.sh):
#   HERON_STAGE_VM/NET/USER/SSH_KEY  VM resolution + ssh (defaults heron-stage
#                                    / default / heron-admin / ~/.ssh/id_ed25519)
#   HERON_STAGE_BIN        binary on the VM to soak   (default /opt/heron/heron)
#   HERON_STAGE_BASELINE   known-good binary on the VM for the dual-binary
#                          self-test (optional; unset → candidate-only soak)
#
# Exit: 0 pass · 1 candidate regressed · 2 setup error. A tara `harness_broken`
# (baseline itself failed → corpus/env problem, never the candidate) is logged
# as a warning and does NOT fail the job.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CORPUS="${1:-$HERE/../../server/h-protocol/tests/fixtures/keepalive_2sse_pipelined.pcap}"
[ -f "$CORPUS" ] || { echo "::error::corpus not found: $CORPUS" >&2; exit 2; }

VM="${HERON_STAGE_VM:-heron-stage}"
NET="${HERON_STAGE_NET:-default}"
SSH_USER="${HERON_STAGE_USER:-heron-admin}"
SSH_KEY="${HERON_STAGE_SSH_KEY:-$HOME/.ssh/id_ed25519}"
STAGE_BIN="${HERON_STAGE_BIN:-/opt/heron/heron}"
BASELINE="${HERON_STAGE_BASELINE:-}"

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

base_arg=""
[ -z "$BASELINE" ] || base_arg="--baseline $BASELINE"

echo "==> running tara soak inside the VM"
set +e
remote "cd $RD && bash tara.sh --binary '$STAGE_BIN' --corpus '$CORPUS_VM' $base_arg --json-out $RD/out.json"
rc=$?
set -e

echo "==> verdict:"
remote "cat $RD/out.json 2>/dev/null" || echo "(no verdict file — tara aborted before writing)"

case "$rc" in
  0) echo "==> SOAK PASS — staging binary healthy under replay"; exit 0;;
  3) echo "::warning::tara reported harness_broken (the baseline failed → corpus/env issue, NOT the candidate) — not failing the job"; exit 0;;
  *) echo "::error::SOAK FAILED — the deployed staging binary regressed (tara rc=$rc)"; exit 1;;
esac
