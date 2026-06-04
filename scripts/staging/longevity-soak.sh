#!/usr/bin/env bash
# longevity-soak — the nightly endurance run (quality-infra L7, PR4).
#
# Drives heron under a steady rate-controlled pcap-file replay for HOURS and
# tracks RSS + on-disk DuckDB size + pipeline metrics over the whole window,
# then asserts the run stayed healthy (no memory leak, no super-linear DB
# bloat, no checkpoint/"broken index" FATAL, no flush errors). This is the test
# that would have caught the 2026-06-02 prod outage — a 102 GB DuckDB whose
# checkpoint hit a "broken index" FATAL → SIGSEGV. Not a per-PR gate; runs from
# a systemd timer (longevity-soak.timer) on the staging VM.
#
# Reuses the same throwaway-workdir + private-port + isolated-DuckDB model as
# tara.sh, plus the rate_pps load primitive (PR1) so the load is steady and
# prod-like instead of an as-fast-as-possible firehose.
#
# Usage:
#   longevity-soak.sh --binary <heron> --corpus <pcap>
#       [--duration 14400] [--rate-pps 500] [--sample-secs 30]
#       [--max-rss-growth-pct 30] [--max-bytes-per-call-growth-pct 50]
#       [--min-pkts N] [--json-out <file>] [--workdir <dir>] [--keep]
#
# Issue filing on regression (best-effort, only when configured):
#   LONGEVITY_REPO   GitHub repo            (default Netis/heron)
#   GH_TOKEN         token for the REST API (no `gh` — the runner has only curl)
#   LONGEVITY_NO_FILE=1   never file an issue (report only)
#
# Exit: 0 healthy · 1 regression · 2 usage/setup error.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/longevity_check.py"

# Main knobs fall back to env (so the systemd unit configures them purely via
# its EnvironmentFile) and are overridable by the flags below.
BINARY="${LONGEVITY_BINARY:-}" CORPUS="${LONGEVITY_CORPUS:-}"
DURATION="${LONGEVITY_DURATION:-14400}" RATE_PPS="${LONGEVITY_RATE_PPS:-500}"
SAMPLE_SECS="${LONGEVITY_SAMPLE_SECS:-30}"
MAX_RSS_GROWTH_PCT=30 MAX_BPC_GROWTH_PCT=50 MIN_PKTS=0
JSON_OUT="" WORKROOT="" KEEP=0 PORT="${LONGEVITY_PORT:-4601}"
while [ $# -gt 0 ]; do
  case "$1" in
    --binary)    BINARY="$2"; shift 2;;
    --corpus)    CORPUS="$2"; shift 2;;
    --duration)  DURATION="$2"; shift 2;;
    --rate-pps)  RATE_PPS="$2"; shift 2;;
    --sample-secs) SAMPLE_SECS="$2"; shift 2;;
    --max-rss-growth-pct) MAX_RSS_GROWTH_PCT="$2"; shift 2;;
    --max-bytes-per-call-growth-pct) MAX_BPC_GROWTH_PCT="$2"; shift 2;;
    --min-pkts)  MIN_PKTS="$2"; shift 2;;
    --json-out)  JSON_OUT="$2"; shift 2;;
    --workdir)   WORKROOT="$2"; shift 2;;
    --port)      PORT="$2"; shift 2;;
    --keep)      KEEP=1; shift;;
    *) echo "longevity-soak: unknown arg '$1'" >&2; exit 2;;
  esac
done

[ -x "$BINARY" ] || { echo "longevity-soak: --binary '$BINARY' missing/not executable" >&2; exit 2; }
[ -f "$CORPUS" ] || { echo "longevity-soak: --corpus '$CORPUS' not found" >&2; exit 2; }
[ -f "$CHECKER" ] || { echo "longevity-soak: checker '$CHECKER' not found" >&2; exit 2; }
# Default the throughput floor to half the nominal rate × duration, so a stalled
# run (no traffic for the window) fails `load_sustained`.
[ "$MIN_PKTS" -gt 0 ] 2>/dev/null || MIN_PKTS=$(( RATE_PPS * DURATION / 2 ))

WORKROOT="${WORKROOT:-$(mktemp -d /tmp/longevity.XXXXXX)}"
mkdir -p "$WORKROOT/data"
cleanup() { [ "$KEEP" = 1 ] || rm -rf "$WORKROOT"; }
trap cleanup EXIT

CORPUS_ABS="$(cd "$(dirname "$CORPUS")" && pwd)/$(basename "$CORPUS")"
CFG="$WORKROOT/config.toml" LOG="$WORKROOT/heron.log"
DB="$WORKROOT/data/heron.duckdb" SAMPLES="$WORKROOT/samples.jsonl"

cat > "$CFG" <<TOML
[[pipeline]]
name = "longevity"
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
source_id = "longevity"
loop_secs = $DURATION
rate_pps = $RATE_PPS
[storage]
backend = "duckdb"
[storage.duckdb]
path = "$DB"
[storage.sink]
batch_size = 200
flush_interval_ms = 500
# Retention enabled with a short sweep interval so the pruning + checkpoint
# paths are exercised across the run (the surfaces that bloated prod).
[storage.retention]
enabled = true
check_interval_secs = 60
calls = 1
turns = 1
http_exchanges = 1
[internal_metrics]
enabled = true
interval_secs = 2
[api]
listen = "127.0.0.1"
port = $PORT
TOML

echo "longevity-soak: starting $BINARY on :$PORT — ${DURATION}s @ ${RATE_PPS}pps, sampling every ${SAMPLE_SECS}s" >&2
"$BINARY" -v -c "$CFG" > "$LOG" 2>&1 &
PID=$!

# Wait for the API.
up=0
for _ in $(seq 1 30); do
  curl -fsS -m 2 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && { up=1; break; }
  kill -0 "$PID" 2>/dev/null || break
  sleep 1
done
if [ "$up" != 1 ]; then
  echo "longevity-soak: heron API never came up" >&2
  kill -9 "$PID" 2>/dev/null || true
  exit 2
fi

# Sample RSS + on-disk DB size + metrics over the window.
: > "$SAMPLES"
t_end=$(( $(date +%s) + DURATION ))
while [ "$(date +%s)" -lt "$t_end" ]; do
  kill -0 "$PID" 2>/dev/null || { echo "longevity-soak: heron died mid-run" >&2; break; }
  rss_kb="$(awk '/^VmRSS:/{print $2}' "/proc/$PID/status" 2>/dev/null || echo 0)"
  db_bytes="$(stat -c %s "$DB" 2>/dev/null || stat -f %z "$DB" 2>/dev/null || echo 0)"
  m="$(curl -fsS -m 5 "http://127.0.0.1:$PORT/api/internal-metrics" 2>/dev/null || echo '{}')"
  printf '{"ts":%s,"rss_kb":%s,"db_bytes":%s,"metrics":%s}\n' \
    "$(date +%s)" "${rss_kb:-0}" "${db_bytes:-0}" "$m" >> "$SAMPLES"
  sleep "$SAMPLE_SECS"
done

# Let the sink drain, then graceful stop.
sleep 4
kill -TERM "$PID" 2>/dev/null || true
for _ in $(seq 1 8); do kill -0 "$PID" 2>/dev/null || break; sleep 1; done
kill -9 "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

VERDICT="$(python3 "$CHECKER" --samples "$SAMPLES" --logfile "$LOG" \
  --max-rss-growth-pct "$MAX_RSS_GROWTH_PCT" \
  --max-bytes-per-call-growth-pct "$MAX_BPC_GROWTH_PCT" \
  --min-pkts "$MIN_PKTS")"
rc=$?
printf '%s\n' "$VERDICT"
[ -z "$JSON_OUT" ] || printf '%s\n' "$VERDICT" > "$JSON_OUT"

if [ "$rc" = 0 ]; then
  echo "longevity-soak: PASS — endured ${DURATION}s with no leak / bloat / FATAL" >&2
  exit 0
fi

echo "longevity-soak: REGRESSION — $(printf '%s' "$VERDICT" | python3 -c 'import json,sys;print(",".join(json.load(sys.stdin).get("failed",[])))' 2>/dev/null)" >&2

# Best-effort: file a scrubbed, de-duplicated issue (only when configured).
REPO="${LONGEVITY_REPO:-Netis/heron}"
if [ "${LONGEVITY_NO_FILE:-0}" = 1 ] || [ -z "${GH_TOKEN:-}" ]; then
  echo "longevity-soak: issue filing skipped (LONGEVITY_NO_FILE or no GH_TOKEN)" >&2
  exit 1
fi

# Mask any internal infra identity before the body leaves the host (same rule
# as mara's scrub()).
scrub() {
  sed -E -e 's/\b[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\b/<ip>/g' \
         -e 's#(/home|/Users)/[A-Za-z0-9._-]+#\1/<user>#g' \
         -e 's#(https?://)[^/[:space:]]+#\1<host>#g'
}
SIG="longevity-regression"
FAILED="$(printf '%s' "$VERDICT" | python3 -c 'import json,sys;print(",".join(json.load(sys.stdin).get("failed",[])))' 2>/dev/null)"
# Dedup: skip if an open issue already tracks this signature.
EXIST="$(curl -fsS -m 10 -H "Authorization: Bearer $GH_TOKEN" \
  "${GITHUB_API_URL:-https://api.github.com}/search/issues?q=$(printf 'repo:%s+is:issue+is:open+in:title+%s' "$REPO" "$SIG")" \
  2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("total_count",0))
except Exception: print(0)' 2>/dev/null || echo 0)"
if [ "${EXIST:-0}" != "0" ]; then
  echo "longevity-soak: open '$SIG' issue already exists — not refiling" >&2
  exit 1
fi

BODY="$(printf '🤖 **longevity-soak** detected an endurance regression.\n\n- **Failed invariants**: `%s`\n- **Duration**: %ss @ %spps\n\n```json\n%s\n```\n\nThis is the leak / DB-bloat / checkpoint-FATAL class (the 102 GB prod-outage family). Add `agent:assess` to route into triage.' \
  "$FAILED" "$DURATION" "$RATE_PPS" "$VERDICT" | scrub)"
TITLE="[longevity] endurance regression: $SIG"
PAYLOAD="$(python3 -c 'import json,sys; print(json.dumps({"title":sys.argv[1],"body":sys.argv[2],"labels":["longevity","incident"]}))' "$TITLE" "$BODY")"
code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json" \
  "${GITHUB_API_URL:-https://api.github.com}/repos/${REPO}/issues" -d "$PAYLOAD" 2>/dev/null || echo 000)
[ "$code" = "201" ] && echo "longevity-soak: filed regression issue" >&2 \
  || echo "::warning::longevity-soak: failed to file issue (HTTP $code)" >&2
exit 1
