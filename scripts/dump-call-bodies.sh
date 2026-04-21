#!/usr/bin/env bash
# scripts/dump-call-bodies.sh
# Usage: scripts/dump-call-bodies.sh <path-to-duckdb-file> <wire_api> <output|input> <out-dir>
set -euo pipefail
DB="$1"; WIRE="$2"; SIDE="$3"; OUT="$4"
mkdir -p "$OUT"
FIELD="response_body"
[ "$SIDE" = "input" ] && FIELD="request_body"
duckdb "$DB" -noheader -list -cmd ".mode json" \
  "SELECT id, $FIELD FROM llm_calls WHERE wire_api = '$WIRE' AND $FIELD IS NOT NULL LIMIT 5" \
  | jq -c '.[]' \
  | while IFS= read -r row; do
      id=$(jq -r '.id' <<<"$row")
      body=$(jq -r ".${FIELD}" <<<"$row")
      printf '%s' "$body" > "$OUT/${WIRE}_${SIDE}_${id}.json"
    done
echo "wrote to $OUT"
