#!/usr/bin/env bash
# Provision the staging libvirt VM — the CI staging-deploy target.
#
# Idempotent-ish: refuses to clobber an existing domain (destroy it first to
# re-provision). Produces a cloud-init Ubuntu VM with:
#   - a `heron` system user + /opt/heron + /var/lib/heron
#   - libpcap runtime + jq + ca-certificates
#   - the heron.service unit (AmbientCapabilities, no setcap) and config.toml
#     from this directory, written in place and `enable`d (NOT started — the
#     binary lands on the first deploy, which starts it).
#
# Nothing internal is hardcoded: the base image, the keys to authorize, and
# the apt proxy all come from the environment so this script is safe to commit.
#
# Required env:
#   BASE_IMAGE                 path to an Ubuntu cloud qcow2 (e.g. noble)
#   SSH_AUTHORIZED_KEYS_FILE   file of public keys (one per line) to authorize
#                              for the VM's login user (deploy runner + ops)
# Optional env:
#   APT_PROXY                  http proxy for apt/egress (omitted if unset)
#   VM_NAME / VM_USER          REQUIRED — the VM domain + login (keep in sync
#                              with repo Variables HERON_STAGE_VM/USER; never
#                              hardcoded in source, per the PR-hygiene rule)
#   VM_RAM_MB (6144)  VM_VCPUS (4)
#   VM_DISK_GB (40)  LIBVIRT_NET (default)  IMAGES_DIR (/var/lib/libvirt/images)
#
# Run on the libvirt host (needs sudo for virsh + the images dir).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

: "${BASE_IMAGE:?set BASE_IMAGE to an Ubuntu cloud qcow2 path}"
: "${SSH_AUTHORIZED_KEYS_FILE:?set SSH_AUTHORIZED_KEYS_FILE to a file of public keys}"
[ -f "$BASE_IMAGE" ] || { echo "BASE_IMAGE not found: $BASE_IMAGE" >&2; exit 1; }
[ -f "$SSH_AUTHORIZED_KEYS_FILE" ] || { echo "keys file not found: $SSH_AUTHORIZED_KEYS_FILE" >&2; exit 1; }

# VM domain + login describe internal topology → never hardcode in source;
# keep these in sync with repo Variables HERON_STAGE_VM / HERON_STAGE_USER.
VM_NAME="${VM_NAME:?set VM_NAME (the libvirt domain; matches HERON_STAGE_VM)}"
VM_USER="${VM_USER:?set VM_USER (the VM login; matches HERON_STAGE_USER)}"
VM_RAM_MB="${VM_RAM_MB:-6144}"
VM_VCPUS="${VM_VCPUS:-4}"
VM_DISK_GB="${VM_DISK_GB:-40}"
LIBVIRT_NET="${LIBVIRT_NET:-default}"
IMAGES_DIR="${IMAGES_DIR:-/var/lib/libvirt/images}"

if sudo virsh dominfo "$VM_NAME" >/dev/null 2>&1; then
  echo "domain '$VM_NAME' already exists — destroy+undefine it first to re-provision." >&2
  exit 1
fi

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

# --- cloud-init: meta-data ---
cat > "$WORK/meta-data" <<EOF
instance-id: ${VM_NAME}-001
local-hostname: ${VM_NAME}
EOF

# --- cloud-init: user-data ---
# Indent embedded files by 6 spaces to sit under the write_files `content: |`.
indent6() { sed 's/^/      /'; }
KEYS_BLOCK="$(while IFS= read -r k; do [ -n "$k" ] && echo "      - $k"; done < "$SSH_AUTHORIZED_KEYS_FILE")"
APT_BLOCK=""
ENV_PROXY_BLOCK=""
if [ -n "${APT_PROXY:-}" ]; then
  APT_BLOCK=$'apt:\n  http_proxy: '"$APT_PROXY"$'\n  https_proxy: '"$APT_PROXY"
  ENV_PROXY_BLOCK="  - path: /etc/environment
    content: |
      HTTP_PROXY=${APT_PROXY}
      HTTPS_PROXY=${APT_PROXY}
      http_proxy=${APT_PROXY}
      https_proxy=${APT_PROXY}
      no_proxy=localhost,127.0.0.1,.local
      NO_PROXY=localhost,127.0.0.1,.local
"
fi

{
cat <<EOF
#cloud-config
hostname: ${VM_NAME}
fqdn: ${VM_NAME}.local
manage_etc_hosts: true

users:
  - name: ${VM_USER}
    sudo: ALL=(ALL) NOPASSWD:ALL
    groups: [sudo, adm]
    shell: /bin/bash
    lock_passwd: true
    ssh_authorized_keys:
${KEYS_BLOCK}

${APT_BLOCK}

package_update: true
package_upgrade: false
packages:
  - ca-certificates
  - curl          # deploy-staging.sh health-gates via curl inside the VM
  - libpcap0.8
  - jq

write_files:
${ENV_PROXY_BLOCK}  - path: /opt/heron/config.toml
    permissions: '0644'
    content: |
$(indent6 < "$HERE/config.toml")
  - path: /etc/systemd/system/heron.service
    permissions: '0644'
    content: |
$(indent6 < "$HERE/heron.service")

runcmd:
  # SHELL strings (not exec-form lists) so '||' works — an exec-form
  # [getent, group, heron, '||', ...] passes '||' as a literal arg and the
  # heron user silently never gets created.
  - "getent group heron || groupadd --system heron"
  - "id -u heron >/dev/null 2>&1 || useradd --system --gid heron --home-dir /var/lib/heron --shell /usr/sbin/nologin heron"
  - mkdir -p /opt/heron /var/lib/heron/data /var/lib/heron/dumps
  - chown -R heron:heron /var/lib/heron
  - systemctl daemon-reload
  # enable (not start): the binary lands on the first deploy, which starts it.
  - systemctl enable heron.service
  - "echo ${VM_NAME} cloud-init complete > /var/lib/heron/.provisioned"
EOF
} > "$WORK/user-data"

echo "==> building seed.iso"
cloud-localds "$WORK/seed.iso" "$WORK/user-data" "$WORK/meta-data"

echo "==> creating ${VM_DISK_GB}G disk from $BASE_IMAGE"
sudo mkdir -p "$IMAGES_DIR/$VM_NAME"
cp "$BASE_IMAGE" "$WORK/disk.qcow2"
qemu-img resize "$WORK/disk.qcow2" "${VM_DISK_GB}G"
sudo cp "$WORK/disk.qcow2" "$WORK/seed.iso" "$IMAGES_DIR/$VM_NAME/"
sudo chown libvirt-qemu:kvm "$IMAGES_DIR/$VM_NAME/disk.qcow2" "$IMAGES_DIR/$VM_NAME/seed.iso"

echo "==> virt-install $VM_NAME"
sudo virt-install \
  --name "$VM_NAME" \
  --memory "$VM_RAM_MB" \
  --vcpus "$VM_VCPUS" \
  --disk "path=$IMAGES_DIR/$VM_NAME/disk.qcow2,format=qcow2,bus=virtio" \
  --disk "path=$IMAGES_DIR/$VM_NAME/seed.iso,device=cdrom" \
  --os-variant ubuntu24.04 \
  --network "network=$LIBVIRT_NET,model=virtio" \
  --graphics none --import --noautoconsole

echo "==> $VM_NAME created. Find its IP with:"
echo "    sudo virsh net-dhcp-leases $LIBVIRT_NET | grep $VM_NAME"
echo "    (cloud-init finishes in a few minutes; /var/lib/heron/.provisioned marks done)"
