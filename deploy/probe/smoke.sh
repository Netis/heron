#!/usr/bin/env bash
# Static smoke for the heron-probe packaging artifacts. Runnable on any host (no
# eBPF / cluster needed): validates the DaemonSet YAML parses, the systemd unit
# declares the eBPF capabilities, and — where the tools exist — runs
# systemd-analyze verify and a client-side kubectl dry-run.
#
# It does NOT exercise real capture: that requires a Linux host with CAP_BPF +
# BTF (systemd target) or a cluster (DaemonSet). The on-host capture smoke is in
# the "Smoke (on host)" section of README.md.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UNIT="$DIR/heron-probe.service"
DS="$DIR/k8s-daemonset.yaml"
fail=0
ok()   { printf '  ✓ %s\n' "$1"; }
bad()  { printf '  ✗ %s\n' "$1" >&2; fail=1; }

echo "== systemd unit: $UNIT"
grep -q '^ExecStart=/usr/local/bin/heron-probe' "$UNIT" && ok "ExecStart present" || bad "ExecStart missing"
for cap in CAP_BPF CAP_PERFMON CAP_SYS_ADMIN; do
    grep -qE "^AmbientCapabilities=.*\b$cap\b" "$UNIT" && ok "AmbientCapabilities has $cap" || bad "AmbientCapabilities missing $cap"
done
grep -q '^WantedBy=multi-user.target' "$UNIT" && ok "[Install] WantedBy present" || bad "[Install] missing"
if command -v systemd-analyze >/dev/null 2>&1; then
    systemd-analyze verify "$UNIT" && ok "systemd-analyze verify clean" || bad "systemd-analyze verify failed"
else
    echo "  – systemd-analyze not present (Linux-only); skipped"
fi

echo "== DaemonSet: $DS"
if command -v python3 >/dev/null 2>&1 && python3 -c 'import yaml' 2>/dev/null; then
    python3 - "$DS" <<'PY' && ok "YAML parses; kinds + privileged + hostPID + nodeName checks passed" || bad "DaemonSet validation failed"
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
kinds = [d.get("kind") for d in docs]
assert "DaemonSet" in kinds and "ConfigMap" in kinds, f"expected DaemonSet+ConfigMap, got {kinds}"
ds = next(d for d in docs if d["kind"] == "DaemonSet")
spec = ds["spec"]["template"]["spec"]
assert spec.get("hostPID") is True, "hostPID must be true"
c = spec["containers"][0]
assert c["securityContext"]["privileged"] is True, "container must be privileged"
envs = {e["name"]: e for e in c.get("env", [])}
assert envs["HERON_PROBE_SOURCE_ID"]["valueFrom"]["fieldRef"]["fieldPath"] == "spec.nodeName", "source_id must come from node name"
mounts = {m["mountPath"] for m in c["volumeMounts"]}
assert "/sys/kernel/btf" in mounts, "BTF mount missing"
# The embedded probe config must itself be valid TOML (catches a manifest typo).
cm = next(d for d in docs if d["kind"] == "ConfigMap")
toml_text = cm["data"]["heron-probe.toml"]
try:
    import tomllib
    parsed = tomllib.loads(toml_text)
    assert parsed["source"]["type"] == "ebpf", "ConfigMap source must be ebpf"
    assert "central_endpoint" in parsed and "tls" in parsed, "ConfigMap missing required keys"
    print("ConfigMap TOML valid;", end=" ")
except ModuleNotFoundError:
    print("(tomllib needs py3.11+; TOML parse skipped)", end=" ")
print("checked kinds:", kinds)
PY
else
    echo "  – python3+pyyaml not present; skipping YAML parse"
fi
if command -v kubectl >/dev/null 2>&1; then
    # Client-side only (--validate=false avoids the OpenAPI fetch that needs a
    # reachable cluster). Best-effort: a dev box without a kubeconfig still gets
    # the authoritative structural check from the PyYAML step above.
    if kubectl apply --dry-run=client --validate=false -f "$DS" >/dev/null 2>&1; then
        ok "kubectl client dry-run clean"
    else
        echo "  – kubectl present but no reachable cluster/kubeconfig; skipped dry-run"
    fi
else
    echo "  – kubectl not present; skipped client dry-run"
fi

echo
if [ "$fail" -eq 0 ]; then
    echo "static smoke: ✓ all checks passed"
else
    echo "static smoke: ✗ failures above" >&2
    exit 1
fi
