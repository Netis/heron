#!/usr/bin/env bash
#
# Storage-backend benchmark: ClickHouse vs DuckDB, run through the identical
# workload via the `storage_bench` binary. Produces a side-by-side comparison
# of write throughput (rows/sec) and read latency (p50/p95 ms).
#
# Run this ON THE HOST where ClickHouse is reachable on loopback so DuckDB
# (embedded) and ClickHouse (server) compete on the same hardware/disk — that
# is the only apples-to-apples engine comparison. The DuckDB file and the
# ClickHouse server should sit on the same storage tier.
#
# Environment:
#   CLICKHOUSE_URL   ClickHouse HTTP endpoint (default http://localhost:8123)
#   CALLS            llm_calls to write (default 200000)
#   TURNS            agent_turns to write (default 40000)
#   METRICS          llm_metrics rows to write (default 100000)
#   BATCH            rows per write batch (default 1000)
#   BODY_BYTES       approx body size per call (default 2048)
#   DUCKDB_PATH      DuckDB file (default a fresh temp file)
#   PROFILE          cargo profile: release (default) or dev
#
# Both backends are built in release by default so the comparison reflects
# optimized DuckDB. The script is host-agnostic — it never hardcodes any
# hostname; point CLICKHOUSE_URL at wherever ClickHouse listens.
set -euo pipefail

cd "$(dirname "$0")/../server"

CLICKHOUSE_URL="${CLICKHOUSE_URL:-http://localhost:8123}"
CALLS="${CALLS:-200000}"
TURNS="${TURNS:-40000}"
METRICS="${METRICS:-100000}"
BATCH="${BATCH:-1000}"
BODY_BYTES="${BODY_BYTES:-2048}"
DUCKDB_PATH="${DUCKDB_PATH:-$(mktemp -u /tmp/heron-bench-XXXXXX.duckdb)}"
PROFILE="${PROFILE:-release}"

PROFILE_FLAG=""
TARGET_DIR="debug"
if [ "$PROFILE" = "release" ]; then
  PROFILE_FLAG="--release"
  TARGET_DIR="release"
fi

echo ">>> building storage_bench ($PROFILE)…" >&2
cargo build $PROFILE_FLAG -p heron --bin storage_bench >&2
BIN="target/$TARGET_DIR/storage_bench"

COMMON=(--calls "$CALLS" --turns "$TURNS" --metrics "$METRICS" --batch "$BATCH" --body-bytes "$BODY_BYTES")

echo ">>> DuckDB ($DUCKDB_PATH)…" >&2
rm -f "$DUCKDB_PATH"
DUCK_JSON=$("$BIN" --backend duckdb --duckdb-path "$DUCKDB_PATH" "${COMMON[@]}")
rm -f "$DUCKDB_PATH"

echo ">>> ClickHouse ($CLICKHOUSE_URL)…" >&2
CH_JSON=$("$BIN" --backend clickhouse --ch-url "$CLICKHOUSE_URL" "${COMMON[@]}")

echo ">>> comparison" >&2
python3 - "$DUCK_JSON" "$CH_JSON" <<'PY'
import json, sys
duck = json.loads(sys.argv[1]); ch = json.loads(sys.argv[2])
print(f"\nworkload: calls={duck['rows']['calls']} metrics={duck['rows']['metrics']} "
      f"turns={duck['rows']['turns']} body={duck['body_bytes']}B batch={duck['batch']}\n")
w = "{:<22}{:>16}{:>16}{:>10}"
print(w.format("write rows/sec", "duckdb", "clickhouse", "CH/Duck"))
for k in ("calls", "metrics", "turns"):
    d = duck["write_rows_per_sec"][k]; c = ch["write_rows_per_sec"][k]
    print(w.format(k, f"{d:,.0f}", f"{c:,.0f}", f"{c/d:.2f}x" if d else "-"))
print()
q = "{:<22}{:>16}{:>16}{:>10}"
print(q.format("query p50 ms", "duckdb", "clickhouse", "CH/Duck"))
for k in duck["query_ms"]:
    d = duck["query_ms"][k]["p50"]; c = ch["query_ms"][k]["p50"]
    print(q.format(k, f"{d:.2f}", f"{c:.2f}", f"{c/d:.2f}x" if d else "-"))
PY
