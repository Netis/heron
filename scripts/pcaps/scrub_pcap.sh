#!/usr/bin/env bash
# Turn a raw capture into a committable, secret-free corpus fixture.
#
#   scripts/pcaps/scrub_pcap.sh <raw.pcap> --id <fixture-id> [--trim <expr>]
#
# Steps:
#   1. (optional) trim to the relevant TCP streams with editcap/tshark IF
#      present — skipped gracefully otherwise.
#   2. length-preserving secret/PII redaction via scrub_pcap.py (the gate:
#      refuses to emit if a forbidden pattern survives).
#   3. write testdata/pcaps/corpus/<id>.pcap + testdata/pcaps/<id>.scrub.json.
#
# After this: add/flip the corpus.toml entry to status="active", then
# `just corpus bless` to generate the golden, eyeball the golden diff, and
# commit (the .pcap lands in git-LFS per .gitattributes).
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

RAW=""
ID=""
TRIM=""
while [ $# -gt 0 ]; do
  case "$1" in
    --id) ID="$2"; shift 2 ;;
    --trim) TRIM="$2"; shift 2 ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) RAW="$1"; shift ;;
  esac
done
[ -n "$RAW" ] && [ -n "$ID" ] || { echo "usage: scrub_pcap.sh <raw.pcap> --id <id> [--trim <tshark-filter>]" >&2; exit 2; }
[ -f "$RAW" ] || { echo "no such file: $RAW" >&2; exit 2; }

CORPUS_DIR="$ROOT/testdata/pcaps/corpus"
OUT="$CORPUS_DIR/$ID.pcap"
SIDE="$ROOT/testdata/pcaps/$ID.scrub.json"
mkdir -p "$CORPUS_DIR"

WORK="$RAW"
TMP=""
if [ -n "$TRIM" ]; then
  if command -v tshark >/dev/null 2>&1; then
    TMP=$(mktemp /tmp/scrub-trim-XXXX.pcap)
    echo "trim: tshark -Y '$TRIM'"
    tshark -r "$RAW" -Y "$TRIM" -w "$TMP" >/dev/null 2>&1 || { echo "tshark trim failed" >&2; exit 1; }
    WORK="$TMP"
  else
    echo "::warning:: --trim requested but tshark not found; skipping trim" >&2
  fi
fi

python3 "$HERE/scrub_pcap.py" "$WORK" "$OUT" --sidecar "$SIDE"
[ -n "$TMP" ] && rm -f "$TMP"

echo
echo "wrote $OUT"
echo "next: set status=\"active\" for id=\"$ID\" in testdata/pcaps/corpus.toml, then 'just corpus bless'"
