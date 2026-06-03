#!/usr/bin/env bash
# tara — staging soak runner (quality-infra L7).
#
# Replays a known pcap corpus through a heron binary using heron's built-in
# `pcap-file` capture source (no NIC, no tcpreplay — deterministic, fast),
# then asserts parse / pairing / turn / persistence invariants from
# /api/internal-metrics (see tara_invariants.py).
#
# Known-good self-test: pass --baseline <last-released heron> and tara runs
# BOTH binaries through the identical corpus. If the baseline fails an
# invariant the HARNESS (corpus/env) is broken — tara reports `harness_broken`
# and does NOT blame the candidate. Only a candidate that fails where the
# baseline passed is a real regression.
#
# Each heron instance runs in a throwaway workdir on a private port with an
# isolated DuckDB, so tara never touches the deployed heron.service / its DB.
# pcap-file reads a file (not a NIC), so NO capture capabilities are needed.
#
# Usage:
#   tara.sh --binary <heron> --corpus <pcap> [--baseline <heron>]
#           [--json-out <file>] [--port <base>] [--workdir <dir>]
#           [--min-reqs N] [--min-turns N] [--keep]
#   Load/soak mode (perf + reliability instead of single-pass correctness):
#   tara.sh --binary <heron> --corpus <pcap> --load [--duration 60]
#           [--rate-pps 1000] [--max-queue-pct 80] [--max-rss-growth-pct 50]
#           [--min-pkts 10000] [--baseline <heron>]
#
# Exit: 0 pass · 1 candidate regressed · 2 usage/setup · 3 harness_broken
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/tara_invariants.py"

BINARY="" CORPUS="" BASELINE="" JSON_OUT="" PORT_BASE=4599 WORKROOT="" KEEP=0
MIN_REQS=1 MIN_TURNS=0
# Load-mode knobs (only used with --load):
LOAD=0 DURATION=60 MAX_QUEUE_PCT=80 MAX_RSS_GROWTH_PCT=50 MIN_PKTS=10000 RATE_PPS=1000
while [ $# -gt 0 ]; do
  case "$1" in
    --binary)    BINARY="$2"; shift 2;;
    --corpus)    CORPUS="$2"; shift 2;;
    --baseline)  BASELINE="$2"; shift 2;;
    --json-out)  JSON_OUT="$2"; shift 2;;
    --port)      PORT_BASE="$2"; shift 2;;
    --workdir)   WORKROOT="$2"; shift 2;;
    --min-reqs)  MIN_REQS="$2"; shift 2;;
    --min-turns) MIN_TURNS="$2"; shift 2;;
    # Load/soak mode: loop the corpus for <duration> s at a steady <rate-pps>
    # and assert perf + reliability invariants (drops, queue depth, RSS growth,
    # throughput) instead of single-pass correctness. Needs a binary with
    # pcap-file loop + rate support (loop_secs / rate_pps).
    --load)      LOAD=1; shift;;
    --duration)  DURATION="$2"; shift 2;;
    --rate-pps)  RATE_PPS="$2"; shift 2;;
    --max-queue-pct)      MAX_QUEUE_PCT="$2"; shift 2;;
    --max-rss-growth-pct) MAX_RSS_GROWTH_PCT="$2"; shift 2;;
    --min-pkts)  MIN_PKTS="$2"; shift 2;;
    --keep)      KEEP=1; shift;;
    *) echo "tara: unknown arg '$1'" >&2; exit 2;;
  esac
done

[ -x "$BINARY" ] || { echo "tara: --binary '$BINARY' missing/not executable" >&2; exit 2; }
[ -f "$CORPUS" ] || { echo "tara: --corpus '$CORPUS' not found" >&2; exit 2; }
[ -f "$CHECKER" ] || { echo "tara: checker '$CHECKER' not found" >&2; exit 2; }
[ -z "$BASELINE" ] || [ -x "$BASELINE" ] || { echo "tara: --baseline '$BASELINE' not executable" >&2; exit 2; }

WORKROOT="${WORKROOT:-$(mktemp -d /tmp/tara.XXXXXX)}"
mkdir -p "$WORKROOT"
cleanup() { [ "$KEEP" = 1 ] || rm -rf "$WORKROOT"; }
trap cleanup EXIT

CORPUS_ABS="$(cd "$(dirname "$CORPUS")" && pwd)/$(basename "$CORPUS")"

# Run one heron instance over the corpus; write the verdict JSON to stdout.
# Args: <binary> <label> <port>
soak_one() {
  local bin="$1" label="$2" port="$3"
  local wd="$WORKROOT/$label"
  mkdir -p "$wd/data"
  local cfg="$wd/config.toml" log="$wd/heron.log" metrics="$wd/metrics.json"
  local samples="$wd/samples.jsonl"
  # In load mode, drive the binary's pcap-file loop replay for the window at a
  # steady rate. rate_pps keeps the load prod-like instead of an as-fast-as-
  # possible firehose that just saturates the channels (which would fail the
  # queues-bounded invariant by design, not by regression).
  local loop_line="" rate_line=""
  if [ "$LOAD" = 1 ]; then
    loop_line="loop_secs = $DURATION"
    rate_line="rate_pps = $RATE_PPS"
  fi

  cat > "$cfg" <<TOML
[[pipeline]]
name = "soak"
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
type = "pcap-file"
path = "$CORPUS_ABS"
realtime = false
source_id = "tara-soak"
$loop_line
$rate_line
[storage]
backend = "duckdb"
[storage.duckdb]
path = "$wd/data/heron.duckdb"
[storage.sink]
batch_size = 200
flush_interval_ms = 500
[internal_metrics]
enabled = true
interval_secs = 2
[api]
listen = "127.0.0.1"
port = $port
TOML

  echo "tara: [$label] starting $bin on :$port (corpus $(basename "$CORPUS_ABS"))" >&2
  "$bin" -v -c "$cfg" > "$log" 2>&1 &
  local pid=$!

  # 1) wait for the API to come up
  local up=0
  for _ in $(seq 1 30); do
    if curl -fsS -m 2 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then up=1; break; fi
    kill -0 "$pid" 2>/dev/null || { echo "tara: [$label] heron exited during startup" >&2; break; }
    sleep 1
  done
  if [ "$up" != 1 ]; then
    echo "{\"label\":\"$label\",\"pass\":false,\"error\":\"heron API never came up\"}"
    kill -9 "$pid" 2>/dev/null || true
    return
  fi

  if [ "$LOAD" = 1 ]; then
    # 2L) sustained-load: sample /api/internal-metrics + process RSS every ~2 s
    # while the binary loops the corpus. The series feeds the load invariants.
    : > "$samples"
    local t_end=$(( $(date +%s) + DURATION ))
    while [ "$(date +%s)" -lt "$t_end" ]; do
      kill -0 "$pid" 2>/dev/null || { echo "tara: [$label] heron died mid-load" >&2; break; }
      local rss_kb m
      rss_kb="$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)"
      m="$(curl -fsS -m 5 "http://127.0.0.1:$port/api/internal-metrics" 2>/dev/null || echo '{}')"
      printf '{"ts":%s,"rss_kb":%s,"metrics":%s}\n' "$(date +%s)" "${rss_kb:-0}" "$m" >> "$samples"
      sleep 2
    done
    # loop_secs has elapsed → the source stops; let the pipeline drain + flush.
    sleep 6
    curl -fsS -m 5 "http://127.0.0.1:$port/api/internal-metrics" > "$metrics" 2>/dev/null \
      || echo '{}' > "$metrics"
  else
    # 2) wait for the corpus to be fully ingested (pcap-file EOF)
    for _ in $(seq 1 60); do
      grep -q "pcap-file: finished reading" "$log" 2>/dev/null && break
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done

    # 3) let turns close (idle+sweep) and the sink flush, then snapshot
    sleep 8
    curl -fsS -m 5 "http://127.0.0.1:$port/api/internal-metrics" > "$metrics" 2>/dev/null \
      || echo '{}' > "$metrics"
  fi

  # 4) graceful stop
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 8); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
  kill -9 "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true

  if [ "$LOAD" = 1 ]; then
    python3 "$CHECKER" --label "$label" --logfile "$log" --metrics-file "$metrics" \
      --load --samples "$samples" --max-queue-pct "$MAX_QUEUE_PCT" \
      --max-rss-growth-pct "$MAX_RSS_GROWTH_PCT" --min-pkts "$MIN_PKTS"
  else
    python3 "$CHECKER" --label "$label" --logfile "$log" \
      --metrics-file "$metrics" --min-reqs "$MIN_REQS" --min-turns "$MIN_TURNS"
  fi
}

BASE_VERDICT="null"
if [ -n "$BASELINE" ]; then
  bv="$(soak_one "$BASELINE" baseline "$PORT_BASE")"
  BASE_VERDICT="$bv"
  if ! printf '%s' "$bv" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("pass") else 1)' 2>/dev/null; then
    # Baseline (last known-good) failed → the corpus or environment is broken,
    # not the candidate. Emit harness_broken and exit neutral-ish (3).
    final="$(python3 - "$BASE_VERDICT" <<'PY'
import json,sys
base=json.loads(sys.argv[1])
print(json.dumps({"pass":False,"reason":"harness_broken","candidate":None,"baseline":base},indent=2))
PY
)"
    printf '%s\n' "$final"
    [ -z "$JSON_OUT" ] || printf '%s\n' "$final" > "$JSON_OUT"
    echo "tara: HARNESS BROKEN — baseline failed [$(printf '%s' "$bv" | python3 -c 'import json,sys;print(",".join(json.load(sys.stdin).get("failed",[])))' 2>/dev/null)]; not blaming candidate" >&2
    exit 3
  fi
fi

CAND_VERDICT="$(soak_one "$BINARY" candidate "$PORT_BASE")"

final="$(python3 - "$CAND_VERDICT" "$BASE_VERDICT" <<'PY'
import json,sys
cand=json.loads(sys.argv[1]); base=json.loads(sys.argv[2])
ok=bool(cand.get("pass"))
print(json.dumps({
  "pass": ok,
  "reason": "ok" if ok else "candidate_regressed",
  "candidate": cand,
  "baseline": base,
}, indent=2))
PY
)"
printf '%s\n' "$final"
[ -z "$JSON_OUT" ] || printf '%s\n' "$final" > "$JSON_OUT"

if printf '%s' "$CAND_VERDICT" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("pass") else 1)' 2>/dev/null; then
  echo "tara: PASS — candidate healthy" >&2
  exit 0
else
  echo "tara: FAIL — candidate regressed [$(printf '%s' "$CAND_VERDICT" | python3 -c 'import json,sys;print(",".join(json.load(sys.stdin).get("failed",[])))' 2>/dev/null)]" >&2
  exit 1
fi
