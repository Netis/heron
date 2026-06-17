#!/usr/bin/env bash
# distributed-soak — large-scale soak for the distributed eBPF capture topology
# (quality-infra L7c). Drives N synthetic `heron-probe`s → one isolated central
# `heron` over mTLS, under sustained load, and asserts the central holds up + all
# probes are correctly attributed (see distributed_invariants.py).
#
# B1 (this script): SYNTHETIC probes — each heron-probe runs a `pcap-file` source
# (the committed corpus, looped at a steady rate; no eBPF, no NIC) and ships over
# mTLS to the central's `thin-probe` listener. This stresses the part most likely
# to break at scale — the central's mTLS fan-in, connection handling, queues, RSS
# and per-source routing — on a single host, cheaply. The eBPF capture path
# itself stays covered by ebpf-soak.sh (single node); B2 (real-eBPF multi-VM
# fidelity) reuses ebpf-soak.sh against a small VM set, wired in
# distributed-soak.yml — not here.
#
# Everything runs in a throwaway workdir on private ports with isolated DuckDBs,
# so it never touches the deployed heron.service / its DB. mTLS certs are
# generated fresh per run (openssl) and never committed.
#
# Usage:
#   distributed-soak.sh --central <heron> --probe <heron-probe> --corpus <pcap>
#       [--probes N] [--duration S] [--rate-pps R] [--json-out F]
#       [--port-base P] [--workdir D] [--keep]
#       [--min-pkts P] [--max-queue-pct Q] [--max-rss-growth-pct G]
#
# Exit: 0 pass · 1 central/fleet regressed · 2 usage/setup
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/distributed_invariants.py"

CENTRAL="" PROBE="" CORPUS="" PROBES=8 DURATION=60 RATE_PPS=300
JSON_OUT="" PORT_BASE=4700 WORKROOT="" KEEP=0
MIN_PKTS=5000 MAX_QUEUE_PCT=80 MAX_RSS_GROWTH_PCT=50
SOURCE_PREFIX="probe-"
while [ $# -gt 0 ]; do
  case "$1" in
    --central)  CENTRAL="$2"; shift 2;;
    --probe)    PROBE="$2"; shift 2;;
    --corpus)   CORPUS="$2"; shift 2;;
    --probes)   PROBES="$2"; shift 2;;
    --duration) DURATION="$2"; shift 2;;
    --rate-pps) RATE_PPS="$2"; shift 2;;
    --json-out) JSON_OUT="$2"; shift 2;;
    --port-base) PORT_BASE="$2"; shift 2;;
    --workdir)  WORKROOT="$2"; shift 2;;
    --min-pkts) MIN_PKTS="$2"; shift 2;;
    --max-queue-pct)      MAX_QUEUE_PCT="$2"; shift 2;;
    --max-rss-growth-pct) MAX_RSS_GROWTH_PCT="$2"; shift 2;;
    --keep)     KEEP=1; shift;;
    *) echo "distributed-soak: unknown arg '$1'" >&2; exit 2;;
  esac
done

[ -x "$CENTRAL" ] || { echo "distributed-soak: --central '$CENTRAL' missing/not executable" >&2; exit 2; }
[ -x "$PROBE" ]   || { echo "distributed-soak: --probe '$PROBE' missing/not executable" >&2; exit 2; }
[ -f "$CORPUS" ]  || { echo "distributed-soak: --corpus '$CORPUS' not found" >&2; exit 2; }
[ -f "$CHECKER" ] || { echo "distributed-soak: checker '$CHECKER' not found" >&2; exit 2; }
command -v openssl >/dev/null 2>&1 || { echo "distributed-soak: openssl required" >&2; exit 2; }

CORPUS_ABS="$(cd "$(dirname "$CORPUS")" && pwd)/$(basename "$CORPUS")"
WORKROOT="${WORKROOT:-$(mktemp -d /tmp/dsoak.XXXXXX)}"
mkdir -p "$WORKROOT"
PROBE_PIDS=()
CENTRAL_PID=""
cleanup() {
  for p in "${PROBE_PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null || true; done
  [ -z "$CENTRAL_PID" ] || { kill -9 "$CENTRAL_PID" 2>/dev/null || true; }
  [ "$KEEP" = 1 ] || rm -rf "$WORKROOT"
}
trap cleanup EXIT

API_PORT=$((PORT_BASE))
THIN_PORT=$((PORT_BASE + 1))

# --- throwaway mTLS PKI (CA → server SAN localhost + one shared client cert) ---
# RSA, not EC: ring's ECDSA loader needs the public key embedded in the PKCS#8
# (rcgen does this; openssl's EC keygen does not → "failed to parse private key").
# RSA-2048 PKCS#8 from openssl loads cleanly in rustls+ring. 1-day throwaway.
gen_pki() {
  local d="$WORKROOT/pki"
  mkdir -p "$d"
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$d/ca.key" -out "$d/ca.crt" -days 1 -subj "/CN=heron-soak-ca" 2>/dev/null
  # server cert (serverAuth EKU + SAN localhost — rustls WebPki requires both)
  openssl req -newkey rsa:2048 -nodes \
    -keyout "$d/server.key" -out "$d/server.csr" -subj "/CN=central" 2>/dev/null
  openssl x509 -req -in "$d/server.csr" -CA "$d/ca.crt" -CAkey "$d/ca.key" \
    -CAcreateserial -out "$d/server.crt" -days 1 \
    -extfile <(printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n') 2>/dev/null
  # one client cert (clientAuth EKU); all probes reuse it — per-probe identity
  # rides in the batch source_id, not the cert.
  openssl req -newkey rsa:2048 -nodes \
    -keyout "$d/client.key" -out "$d/client.csr" -subj "/CN=probe" 2>/dev/null
  openssl x509 -req -in "$d/client.csr" -CA "$d/ca.crt" -CAkey "$d/ca.key" \
    -CAcreateserial -out "$d/client.crt" -days 1 \
    -extfile <(printf 'extendedKeyUsage=clientAuth\n') 2>/dev/null
  [ -s "$d/server.crt" ] && [ -s "$d/client.crt" ] || { echo "distributed-soak: PKI generation failed" >&2; exit 2; }
}

start_central() {
  local d="$WORKROOT/central"
  mkdir -p "$d/data"
  cat > "$d/config.toml" <<TOML
[[pipeline]]
name = "central"
dispatcher_count = 1
flow_shard_count = 4
[pipeline.turn]
idle_timeout_secs = 4
sweep_interval_secs = 2
grace_ms = 500
shard_count = 1
[pipeline.metrics]
shard_count = 1
[[pipeline.sources]]
type = "thin-probe"
listen = "127.0.0.1:$THIN_PORT"
tls = { cert = "$WORKROOT/pki/server.crt", key = "$WORKROOT/pki/server.key", client_ca = "$WORKROOT/pki/ca.crt" }
[storage]
backend = "duckdb"
[storage.duckdb]
path = "$d/data/heron.duckdb"
[storage.sink]
batch_size = 200
flush_interval_ms = 500
[internal_metrics]
enabled = true
interval_secs = 2
[api]
listen = "127.0.0.1"
port = $API_PORT
TOML
  echo "distributed-soak: starting central on api :$API_PORT, thin-probe :$THIN_PORT" >&2
  "$CENTRAL" -v -c "$d/config.toml" > "$d/heron.log" 2>&1 &
  CENTRAL_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS -m 2 "http://127.0.0.1:$API_PORT/api/health" >/dev/null 2>&1 && return 0
    kill -0 "$CENTRAL_PID" 2>/dev/null || { echo "distributed-soak: central exited during startup" >&2; return 1; }
    sleep 1
  done
  echo "distributed-soak: central API never came up" >&2
  return 1
}

start_probe() {
  local i="$1"
  local d="$WORKROOT/probe-$i"
  mkdir -p "$d"
  cat > "$d/heron-probe.toml" <<TOML
central_endpoint = "127.0.0.1:$THIN_PORT"
server_name = "localhost"
source_id = "${SOURCE_PREFIX}$i"
[tls]
cert = "$WORKROOT/pki/client.crt"
key = "$WORKROOT/pki/client.key"
server_ca = "$WORKROOT/pki/ca.crt"
[source]
type = "pcap-file"
path = "$CORPUS_ABS"
loop_secs = $DURATION
rate_pps = $RATE_PPS
source_id = "${SOURCE_PREFIX}$i"
[batching]
max_packets = 256
flush_ms = 100
TOML
  "$PROBE" -c "$d/heron-probe.toml" > "$d/probe.log" 2>&1 &
  PROBE_PIDS+=("$!")
}

# --- run ---------------------------------------------------------------------
gen_pki
start_central || { echo '{"pass":false,"error":"central failed to start"}'; exit 1; }

echo "distributed-soak: launching $PROBES probes (corpus $(basename "$CORPUS_ABS"), ${DURATION}s @ ${RATE_PPS}pps)" >&2
for i in $(seq 0 $((PROBES - 1))); do start_probe "$i"; done

# Sample the CENTRAL's metrics + RSS every 2s for the load window.
SAMPLES="$WORKROOT/samples.jsonl"; : > "$SAMPLES"
METRICS="$WORKROOT/metrics.json"
t_end=$(( $(date +%s) + DURATION ))
while [ "$(date +%s)" -lt "$t_end" ]; do
  kill -0 "$CENTRAL_PID" 2>/dev/null || { echo "distributed-soak: central died mid-load" >&2; break; }
  rss_kb="$(awk '/^VmRSS:/{print $2}' "/proc/$CENTRAL_PID/status" 2>/dev/null || echo 0)"
  m="$(curl -fsS -m 5 "http://127.0.0.1:$API_PORT/api/internal-metrics" 2>/dev/null || echo '{}')"
  printf '{"ts":%s,"rss_kb":%s,"metrics":%s}\n' "$(date +%s)" "${rss_kb:-0}" "$m" >> "$SAMPLES"
  sleep 2
done

# Probes' loop_secs elapsed → they stop; let the central drain + flush.
sleep 6
curl -fsS -m 5 "http://127.0.0.1:$API_PORT/api/internal-metrics" > "$METRICS" 2>/dev/null || echo '{}' > "$METRICS"

# Build the per-source fleet view from /api/agent-turns: recursively count every
# source_id the central attributed calls/turns to. Shape-agnostic (scans for
# "source_id" values) so it survives API field-layout changes.
TURNS="$WORKROOT/agent-turns.json"
# `start`/`end` are REQUIRED and in SECONDS (capped at year 2100 = 4102444800);
# use the widest accepted window so looped-pcap turns (which carry the capture's
# original timestamps) are all included. The response is enveloped
# ({code,message,data:{total,items:[…]}}); the source_id scan below walks it whole.
curl -fsS -m 10 \
  "http://127.0.0.1:$API_PORT/api/agent-turns?start=0&end=4102444800&page=1&page_size=100000" \
  > "$TURNS" 2>/dev/null || echo '{}' > "$TURNS"
SOURCES="$WORKROOT/sources.json"
python3 - "$TURNS" "$SOURCES" <<'PY'
import json, sys
try:
    doc = json.load(open(sys.argv[1]))
except Exception:
    doc = {}
counts = {}
def walk(o):
    if isinstance(o, dict):
        sid = o.get("source_id")
        if isinstance(sid, str) and sid:
            counts.setdefault(sid, {"calls": 0})["calls"] += 1
        for v in o.values():
            walk(v)
    elif isinstance(o, list):
        for v in o:
            walk(v)
walk(doc)
json.dump({"source_ids": counts}, open(sys.argv[2], "w"))
PY

# Stop everything before judging.
for p in "${PROBE_PIDS[@]:-}"; do kill -TERM "$p" 2>/dev/null || true; done
kill -TERM "$CENTRAL_PID" 2>/dev/null || true
for _ in $(seq 1 8); do kill -0 "$CENTRAL_PID" 2>/dev/null || break; sleep 1; done
kill -9 "$CENTRAL_PID" 2>/dev/null || true
CENTRAL_PID=""

VERDICT="$WORKROOT/verdict.json"
python3 "$CHECKER" --label distributed-soak \
  --logfile "$WORKROOT/central/heron.log" \
  --metrics-file "$METRICS" --samples "$SAMPLES" \
  --sources-file "$SOURCES" \
  --expected-probes "$PROBES" --source-prefix "$SOURCE_PREFIX" \
  --min-pkts "$MIN_PKTS" --max-queue-pct "$MAX_QUEUE_PCT" \
  --max-rss-growth-pct "$MAX_RSS_GROWTH_PCT" | tee "$VERDICT"
rc=${PIPESTATUS[0]}

[ -z "$JSON_OUT" ] || cp "$VERDICT" "$JSON_OUT"
if [ "$rc" = 0 ]; then
  echo "distributed-soak: PASS — central healthy, all $PROBES probes attributed" >&2
else
  echo "distributed-soak: FAIL [$(python3 -c 'import json,sys;print(",".join(json.load(open(sys.argv[1])).get("failed",[])))' "$VERDICT" 2>/dev/null)]" >&2
fi
exit "$rc"
