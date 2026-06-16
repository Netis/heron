# heron-probe packaging

Deploy `heron-probe` — the edge eBPF capture agent — onto the hosts whose LLM
API traffic you want to observe. It attaches `SSL_read`/`SSL_write` uprobes,
synthesizes `RawPacket`s (process attribution included), and ships them to a
central `heron` over mTLS. The heavy wire-API decoding stays central, so a probe
fleet rarely needs upgrading.

Two targets, same binary:

| Target | Artifact |
|---|---|
| Inference / gateway server (systemd) | `heron-probe.service` + the static binary |
| Kubernetes (one pod per node) | `Dockerfile` + `k8s-daemonset.yaml` |

> **Runtime requirements (both):** Linux, `CAP_BPF` + `CAP_PERFMON` + kernel BTF
> (`/sys/kernel/btf/vmlinux`). On kernels older than ~5.16 with a hardened
> `perf_event_paranoid`, also `CAP_SYS_ADMIN`. The probe must reach the central's
> `thin-probe` listener (it dials out — NAT/firewall friendly).

## 1. Build the binary (`--features ebpf`)

The eBPF engine is off by default; build it in with the `ebpf` feature. That
needs the BPF toolchain (nightly + `rust-src` + `bpf-linker`):

```sh
rustup toolchain install nightly --component rust-src
cargo install bpf-linker --locked
cd server && cargo build --release --bin heron-probe --features ebpf
# → server/target/release/heron-probe  (Linux only)
```

For Kubernetes, build the container image instead (bundles the toolchain):

```sh
# from the repo root
docker build -f deploy/probe/Dockerfile -t <registry>/heron-probe:<tag> .
docker push <registry>/heron-probe:<tag>
```

## 2a. systemd (inference / gateway server)

```sh
# binary + dirs + unprivileged service user
sudo install -m0755 server/target/release/heron-probe /usr/local/bin/heron-probe
sudo useradd --system --no-create-home --shell /usr/sbin/nologin heron-probe || true
sudo install -d -o heron-probe -g heron-probe /etc/heron-probe

# config + mTLS material (operator-provided; keep keys 600)
sudo cp server/config/heron-probe.example.toml /etc/heron-probe/heron-probe.toml   # then edit
sudo install -m0600 client.crt client.key ca.crt /etc/heron-probe/

# unit
sudo cp deploy/probe/heron-probe.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now heron-probe
```

The unit grants the eBPF capabilities ambiently (`AmbientCapabilities=CAP_BPF
CAP_PERFMON CAP_SYS_ADMIN`), so the process otherwise runs as the unprivileged
`heron-probe` user — no full root. Drop `CAP_SYS_ADMIN` if your kernel doesn't
require it.

**Alternative — no systemd:** grant the binary the caps directly and run it
yourself:

```sh
sudo setcap cap_bpf,cap_perfmon,cap_sys_admin=eip /usr/local/bin/heron-probe
heron-probe --config /etc/heron-probe/heron-probe.toml
```

## 2b. Kubernetes (DaemonSet)

```sh
kubectl create namespace heron
# mTLS material as a Secret (the DaemonSet mounts it at /etc/heron-probe/tls)
kubectl -n heron create secret generic heron-probe-tls \
  --from-file=client.crt --from-file=client.key --from-file=ca.crt
# edit the image + central_endpoint placeholders, then apply
kubectl apply -f deploy/probe/k8s-daemonset.yaml
```

The pod is `privileged` with `hostPID: true` and mounts the node's
`/sys/kernel/btf`. Each pod's `source_id` is its node name, injected via the
Downward API (`HERON_PROBE_SOURCE_ID` ← `spec.nodeName`), so the central console
attributes calls per node. Edge redaction is enabled in the bundled config.

## 3. Smoke

**Static (any host — no eBPF/cluster):** validates the unit's capabilities, the
DaemonSet structure, and the embedded config:

```sh
deploy/probe/smoke.sh
```

**On host (real capture — Linux + CAP_BPF + a reachable central):**

1. Start the probe (systemd: `systemctl status heron-probe`; K8s: `kubectl -n
   heron rollout status ds/heron-probe`). Logs should show
   `heron-probe: uplink connected`.
2. Generate traffic on the host: run an LLM client (e.g. `claude`, or
   `curl https://api.anthropic.com/...`).
3. On the central, confirm the calls arrive **with process attribution**:
   - `GET /api/agent-turns` shows turns whose calls carry `process_pid/comm/exe`,
     `source_id` = the probe's identity (node name / cert CN).
   - `GET /api/internal-metrics` shows the `capture` group's `batches_received` /
     `pkts_received` climbing with no abnormal drops.
4. With redaction enabled, confirm a captured request body / headers show masked
   credentials (`Authorization: ****`, `sk-****`) — never the real key.

> Real eBPF/K8s capture can only be smoked on a Linux host / cluster; it cannot
> run on a macOS dev box. `smoke.sh` covers everything that can be validated
> offline.
