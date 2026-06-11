#!/usr/bin/env bash
# Deploy a freshly-built heron binary to the staging libvirt VM and gate
# on health, rolling back on failure.
#
# Runs on the staging deploy runner (label `staging-deploy`). The staging VM
# lives in the host's `default` libvirt network, so the runner reaches it
# directly — no cross-host hop. All host-specific values (VM name, ssh user,
# key, ports) are env-overridable so nothing internal is baked into the repo.
#
# Usage:
#   scripts/staging/deploy-staging.sh <path-to-heron-binary>
#
# Env (all optional, with safe defaults):
#   HERON_STAGE_VM       REQUIRED  libvirt domain name (CI: repo Variable
#                                  vars.HERON_STAGE_VM — never hardcoded)
#   HERON_STAGE_USER     REQUIRED  ssh login on the VM (CI: vars.HERON_STAGE_USER)
#   HERON_STAGE_NET      libvirt network for the lease   (default: default)
#   HERON_STAGE_SSH_KEY  ssh identity                    (default: ~/.ssh/id_ed25519)
#   HERON_STAGE_PORT     heron API port inside the VM    (default: 4500)
#   HEALTH_TIMEOUT_SECS  health-gate budget              (default: 90)
#
# Exit status: 0 = deployed + healthy; non-zero = failed (and rolled back if a
# previous binary was present).
set -euo pipefail

BIN="${1:?usage: deploy-staging.sh <heron-binary>}"
[ -f "$BIN" ] || { echo "::error::binary not found: $BIN" >&2; exit 1; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The service unit + config.toml are baked into the VM by provision-vm.sh at
# creation time, so a config/unit change (e.g. adding the eBPF source or the
# CAP_BPF/CAP_PERFMON caps) would otherwise need a full re-provision to land.
# Sync them on every deploy instead — the repo is the source of truth — so the
# staging VM always runs exactly the unit + config in this commit.
UNIT_SRC="$HERE/heron.service"
CONF_SRC="$HERE/config.toml"

# VM name + ssh login describe internal topology → never hardcode them in
# source; CI supplies them from repo Variables (see deploy-staging.yml).
VM="${HERON_STAGE_VM:?set HERON_STAGE_VM (libvirt domain; CI passes vars.HERON_STAGE_VM)}"
SSH_USER="${HERON_STAGE_USER:?set HERON_STAGE_USER (VM ssh login; CI passes vars.HERON_STAGE_USER)}"
NET="${HERON_STAGE_NET:-default}"
SSH_KEY="${HERON_STAGE_SSH_KEY:-$HOME/.ssh/id_ed25519}"
PORT="${HERON_STAGE_PORT:-4500}"
HEALTH_TIMEOUT_SECS="${HEALTH_TIMEOUT_SECS:-90}"

# StrictHostKeyChecking=no + a throwaway known_hosts: the target is an
# ephemeral VM on a controlled internal libvirt network whose host key
# changes on every reprovision (and whose DHCP IP can be reused by a
# different VM), so pinning the key would just wedge the deploy. The trust
# boundary is the libvirt network + the deploy key, not TOFU.
SSH_OPTS=(-i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=8)
remote() { ssh "${SSH_OPTS[@]}" "$SSH_USER@$IP" "$@"; }

echo "==> resolving $VM IP from libvirt '$NET' DHCP leases"
IP="$(sudo virsh net-dhcp-leases "$NET" 2>/dev/null | awk -v n="$VM" '$0 ~ n {print $5}' | cut -d/ -f1 | head -1)"
[ -n "$IP" ] || { echo "::error::no DHCP lease for domain '$VM' on net '$NET' — is the VM running?" >&2; exit 1; }
echo "    $VM -> $IP"

echo "==> uploading new binary + unit + config, smoke-running the binary"
scp "${SSH_OPTS[@]}" "$BIN" "$SSH_USER@$IP:/tmp/heron-new"
scp "${SSH_OPTS[@]}" "$UNIT_SRC" "$SSH_USER@$IP:/tmp/heron.service-new"
scp "${SSH_OPTS[@]}" "$CONF_SRC" "$SSH_USER@$IP:/tmp/heron-config.toml-new"
remote 'chmod +x /tmp/heron-new && /tmp/heron-new --version' \
  || { echo "::error::uploaded binary does not run on the VM (glibc/lib mismatch?)" >&2; exit 1; }

echo "==> installing binary + unit + config, restarting (all three backed up)"
# Back up binary, unit, and config symmetrically so the health-gate rollback
# can restore the exact prior state (a new ebpf config/caps that wedges startup
# must roll back config+unit too, not just the binary).
remote 'set -e
  sudo install -d -m 0755 /opt/heron
  if [ -f /opt/heron/heron ]; then sudo cp -f /opt/heron/heron /opt/heron/heron.bak; fi
  if [ -f /opt/heron/config.toml ]; then sudo cp -f /opt/heron/config.toml /opt/heron/config.toml.bak; fi
  if [ -f /etc/systemd/system/heron.service ]; then sudo cp -f /etc/systemd/system/heron.service /opt/heron/heron.service.bak; fi
  sudo install -m 0755 /tmp/heron-new /opt/heron/heron
  sudo install -m 0644 /tmp/heron-config.toml-new /opt/heron/config.toml
  sudo install -m 0644 /tmp/heron.service-new /etc/systemd/system/heron.service
  sudo systemctl daemon-reload
  sudo systemctl restart heron.service'

echo "==> health gate (<= ${HEALTH_TIMEOUT_SECS}s): status=ready AND pipeline running"
deadline=$(( $(date +%s) + HEALTH_TIMEOUT_SECS ))
ok=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  # jq runs inside the VM (curl + jq are present there); the host only string-matches.
  res="$(remote "curl -s -m 5 http://127.0.0.1:${PORT}/api/health | jq -r '(.data.status)+\"|\"+(.data.pipelines[0].running|tostring)'" 2>/dev/null || true)"
  if [ "${res%%|*}" = "ready" ] && [ "${res##*|}" = "true" ]; then ok=1; break; fi
  sleep 5
done

if [ "$ok" = 1 ]; then
  echo "==> OK heron-stage healthy on ${IP}:${PORT} (status=ready, capturing)"
  remote 'rm -f /tmp/heron-new /tmp/heron.service-new /tmp/heron-config.toml-new
    sudo rm -f /opt/heron/heron.bak /opt/heron/config.toml.bak /opt/heron/heron.service.bak' || true
  exit 0
fi

echo "::error::health gate FAILED after ${HEALTH_TIMEOUT_SECS}s — rolling back"
remote 'set -e
  if [ -f /opt/heron/heron.bak ]; then
    sudo install -m 0755 /opt/heron/heron.bak /opt/heron/heron
    [ -f /opt/heron/config.toml.bak ] && sudo install -m 0644 /opt/heron/config.toml.bak /opt/heron/config.toml
    [ -f /opt/heron/heron.service.bak ] && sudo install -m 0644 /opt/heron/heron.service.bak /etc/systemd/system/heron.service
    sudo systemctl daemon-reload
    sudo systemctl restart heron.service
    echo "rolled back binary + config + unit to the previous state"
  else
    echo "no /opt/heron/heron.bak to roll back to (first deploy?)"
  fi'
exit 1
