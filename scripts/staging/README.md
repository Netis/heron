# Staging auto-deploy

Continuous deploy of `heron` to a long-lived **staging VM** after every
successful `main` CI run. This is layer L6 of the quality chain: catch
"builds and tests pass but the real binary doesn't come up healthy" before
it reaches production.

## Topology

```
push → main
   │
   ▼
ci.yml  (runner: self-hosted, heron)
   ├─ test / lint / bun test               (every push + PR)
   └─ on main: bun run build               build the deployable artifact
              + cargo build --release --features console
              + upload-artifact heron-staging
   │
   ▼  workflow_run: ci completed, conclusion == success, branch == main
deploy-staging.yml  (runner: self-hosted, staging-deploy)
   ├─ download-artifact heron-staging  (from the triggering CI run)
   └─ deploy-staging.sh artifact/heron
          ├─ resolve heron-stage VM IP from libvirt DHCP leases
          ├─ scp binary → VM, smoke-run --version
          ├─ back up current binary, install, restart heron.service
          ├─ health gate: status=ready AND pipeline running (≤90s)
          └─ rollback to the backup if the gate fails
```

### Why a separate `staging-deploy` runner

The staging VM lives in the deploy runner host's `default` libvirt network, so
a runner on that host reaches it directly — no cross-host hop, no NAT
port-forwarding. The runner is labelled `staging-deploy` (NOT `heron`), and
`deploy-staging.yml` only triggers on **successful main-branch** CI via
`workflow_run`. PR/fork code runs on the `heron` runner and never lands on
the deploy host.

## The staging VM (`heron-stage`)

- Ubuntu cloud image, libvirt `default` NAT network, provisioned by
  [`provision-vm.sh`](provision-vm.sh).
- `heron` runs as a systemd unit ([`heron.service`](heron.service)) under a
  dedicated `heron` user, with **`AmbientCapabilities=CAP_NET_RAW
  CAP_NET_ADMIN`** — capture works with no `setcap`, so a rebuilt binary
  can't silently degrade to API-only. (Recommended for production too.)
- Binary at `/opt/heron/heron`, config [`config.toml`](config.toml) at
  `/opt/heron/config.toml`, state under `/var/lib/heron`.

### Re-provisioning

```bash
BASE_IMAGE=/path/to/ubuntu-noble-cloudimg-amd64.img \
SSH_AUTHORIZED_KEYS_FILE=/path/to/authorized_keys \
APT_PROXY=http://your-proxy:port \
  scripts/staging/provision-vm.sh
```

`SSH_AUTHORIZED_KEYS_FILE` must include the **deploy runner host's** public
key (the runner SSHes into the VM as `$HERON_STAGE_USER`). `APT_PROXY` is
optional — omit it on a host with direct egress.

## Manual deploy (debugging)

From the deploy host, with the artifact in hand:

```bash
scripts/staging/deploy-staging.sh /path/to/heron
# env knobs: HERON_STAGE_VM, HERON_STAGE_USER, HERON_STAGE_SSH_KEY,
#            HERON_STAGE_PORT, HEALTH_TIMEOUT_SECS
```

## Not included yet

A replay-server feed (recorded wire bytes → the VM's NIC) so staging
exercises the full parse/extract path under load, plus a soak runner that
diffs a known-good binary against the candidate. Tracked for a follow-on.
