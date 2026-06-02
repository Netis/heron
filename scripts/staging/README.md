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
   │
   ▼  workflow_run: deploy-staging completed, conclusion == success
staging-soak.yml  (runner: self-hosted, staging-deploy)
   └─ soak-staging.sh
          ├─ resolve heron-stage VM IP, scp tara + corpus into the VM
          └─ tara.sh: replay a known pcap through the deployed binary
                      (pcap-file source) and assert parse/pairing/turn/
                      persistence invariants — fail the job on regression
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

## Soak runner (`tara`) — L7

After a deploy is healthy, **tara** replays a known pcap corpus through the
deployed binary and asserts that it still parses real wire traffic correctly.
This catches the regression class that compiles and passes unit tests but
mangles live traffic — the PR#47 (validator bypassed by a direct caller) and
PR#48 (storage poisoning under load) families.

How it works — no NIC, no `tcpreplay`:

- [`tara.sh`](tara.sh) starts a throwaway heron instance on a private port
  with an isolated DuckDB, pointed at the corpus via heron's built-in
  **`pcap-file`** capture source (`type = "pcap-file"`). It never touches the
  deployed `heron.service` or its DB. Because `pcap-file` reads a file (not a
  device), the soak needs **no capture capabilities**.
- After ingest (pcap EOF) it snapshots `/api/internal-metrics` and runs
  [`tara_invariants.py`](tara_invariants.py), which asserts: corpus fully
  ingested, no FATAL/panic, zero malformed/read errors, every routed packet
  parsed, HTTP exchanges all paired, LLM calls detected + ingested, turns
  built, no late drops.
- **Known-good self-test**: pass `--baseline <last-good heron>` and tara runs
  *both* binaries through the identical corpus. If the baseline fails an
  invariant, tara reports `harness_broken` (exit 3) and does **not** blame the
  candidate — so a bad corpus or flaky environment never red-flags a good
  build. A candidate that fails where the baseline passed is a real regression
  (exit 1).

### Known-good promotion (the self-test is standing, not one-off)

`soak-staging.sh` makes the dual-binary self-test continuous via a **rolling
known-good**: the baseline is `/opt/heron/heron.last-good` on the VM — the
last binary that *passed* a soak. Each deploy is soaked against it, and on a
pass the freshly-deployed binary is **promoted** to become the next
known-good. So every new build is compared against the previous good build and
the pointer advances on its own — no stale, hand-pinned baseline.

```
deploy N   → soak vs last-good(N-1) → pass → promote: last-good := N
deploy N+1 → soak vs last-good(N)   → …
```

- First ever run (no known-good): candidate-only **bootstrap**, then promote.
- Candidate regresses (exit 1): job fails, **known-good unchanged**.
- Baseline itself fails (`harness_broken`, exit 3): corpus/env problem, or the
  known-good needs re-baselining → warn, don't fail, **don't advance**.

Override knobs: `HERON_STAGE_BASELINE` pins an explicit baseline (debug),
`HERON_STAGE_NO_PROMOTE=1` soaks without advancing,
`HERON_STAGE_LASTGOOD` relocates the pointer.

The invariant logic is unit-tested (stdlib-only, runs in CI):
`python3 scripts/staging/tests/test_tara_invariants.py`.

### Manual soak (debugging)

```bash
# Against the deployed VM binary, from the deploy host:
scripts/staging/soak-staging.sh                 # uses the committed fixture
scripts/staging/soak-staging.sh /path/corpus.pcap
#   env: HERON_STAGE_VM/USER/SSH_KEY, HERON_STAGE_BIN, HERON_STAGE_BASELINE

# Or directly, given a binary + corpus on the same host:
scripts/staging/tara.sh --binary ./heron \
  --corpus server/h-protocol/tests/fixtures/keepalive_2sse_pipelined.pcap \
  --baseline ./heron-last-good --json-out soak.json
```

The corpus is the committed `keepalive_2sse_pipelined.pcap` fixture — small,
deterministic, ships with the repo. A richer/larger corpus (real scrubbed
dumps, or LLM-synthesized fixtures) is a later enrichment; the dual-binary
self-test makes swapping the corpus safe.
