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
   ├─ download-artifact heron-staging  (built with --features "console ebpf")
   └─ deploy-staging.sh artifact/heron
          ├─ resolve heron-stage VM IP from libvirt DHCP leases
          ├─ scp binary + heron.service + config.toml → VM, smoke-run --version
          ├─ back up current binary+unit+config, install, daemon-reload, restart
          ├─ health gate: status=ready AND pipeline running (≤90s)
          └─ rollback (binary+unit+config) to the backup if the gate fails
   │
   ▼  workflow_run: deploy-staging completed, conclusion == success
   ├──────────────────────────────┐
staging-soak.yml                ebpf-soak.yml   (both: self-hosted, staging-deploy)
   └─ soak-staging.sh              └─ ebpf-soak.sh
       ├─ scp tara + corpus            ├─ assert deployed binary is an eBPF build
       └─ tara.sh: replay a known      ├─ generate LLM-shaped TLS traffic on the VM
          pcap through the deployed     │  (ebpf_soak_gen.py via openssl s_client)
          binary, assert parse/         └─ assert ebpf_* metrics moved AND a
          pairing/turn/persistence         process-attributed LlmCall was
          invariants → stamp               synthesized+parsed+persisted → stamp
          `staging-soaked`                 `ebpf-soaked`
```

Prod promotion requires **both** the `staging-soaked` **and** `ebpf-soaked`
commit statuses to be green (enforced in `deploy-prod.yml` post-approval and in
`release.yml`'s pre-tag gate).

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
  dedicated `heron` user, with **`AmbientCapabilities=CAP_NET_RAW CAP_NET_ADMIN
  CAP_BPF CAP_PERFMON`** — pcap capture works with no `setcap`, and the
  CAP_BPF/CAP_PERFMON pair lets the eBPF source load its BPF program + attach
  SSL uprobes. (Recommended for production too.)
- The [`config.toml`](config.toml) runs both a `pcap` source and an `ebpf`
  source (autodetects `libssl`), so a deploy exercises the on-host SSL-uprobe
  path that `ebpf-soak` then validates.
- Binary at `/opt/heron/heron`, config at `/opt/heron/config.toml`, state under
  `/var/lib/heron`. **The unit + config are synced from this repo on every
  deploy** (`deploy-staging.sh`), so a change here lands without a re-provision.

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

**Prod go/no-go signal.** On every real soak the workflow stamps a
`staging-soaked` **commit status** on the soaked commit — `success` when the
soak passed, `failure` when it didn't. This is the single authoritative thing
to check before approving the `production` deployment: a commit showing
`staging-soaked ✅` cleared `ci → deploy-staging → staging-soak` end to end; a
commit with **no** `staging-soaked` status was never actually soaked (its
chain skipped because CI wasn't green yet) — don't promote it. Don't rely on
the mere existence of a pending `deploy-prod` run: during a merge burst those
appear and get superseded/cancelled, and a skipped chain can't reach the
approval gate anyway (`deploy-prod`'s `if` requires a *successful* soak).

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

## eBPF soak (`ebpf-soak.sh`) — L7b

Where the tara soak proves the **pcap** path, the eBPF soak proves the
**on-host SSL-uprobe** path end to end on the staging VM, so the encrypted-API
capture feature is never promoted to prod un-validated. It runs in parallel
with the tara soak after each `deploy-staging` and stamps an `ebpf-soaked`
commit status (same go/no-go mechanism as `staging-soaked`).

```
ebpf-soak.sh
   ├─ assert the deployed binary is an eBPF build (runtime-config.ebpf_available)
   ├─ snapshot baseline ebpf_* counters
   ├─ ebpf_soak_gen.py on the VM: an HTTPS stub serving /v1/chat/completions +
   │     N requests driven through `openssl s_client` (the canonical libssl
   │     client — curl/Python ssl may call SSL_*_ex or link GnuTLS and never
   │     trip the SSL_read/SSL_write uprobes)
   └─ poll the API until ALL hold, else fail (do not promote):
         ebpf_uprobes_attached  ≥ 1        (attached to libssl)
         ebpf_events_received   delta > 0  (uprobe fired)
         ebpf_frames_synthesized delta > 0 (fed the pipeline)
         ebpf_process_cache_size ≥ 1       (pid/comm attribution ran)
         LlmCall(/v1/chat/completions) ≥ 1 (synth → parse → persist worked)
```

Process attribution is asserted via the `ebpf_process_cache_size` gauge because
the `/api/llm-calls` list view doesn't surface the process columns yet (storage
persists `process_pid/comm/exe`; surfacing them is a later phase).

### Manual eBPF soak (debugging)

```bash
# From the deploy host, against the deployed VM binary:
scripts/staging/ebpf-soak.sh
#   env: HERON_STAGE_VM/USER/SSH_KEY, HERON_STAGE_API_PORT,
#        HERON_EBPF_REQUESTS (default 8), HERON_EBPF_POLL_SECS (default 60)
```

Needs a VM whose `heron` was built with `--features "console ebpf"` (CI's
staging artifact is) and whose `heron.service` grants `CAP_BPF`+`CAP_PERFMON`.
