#!/usr/bin/env bash
# Corpus integrity + secret gate. `check-leakage.sh` skips binary files
# (grep -I), so committed .pcap fixtures are NOT scanned by it — THIS is the
# secret guard for the binary corpus, plus manifest↔file consistency.
#
# Checks:
#   1. Every active corpus.toml entry has a committed corpus/<file> and a
#      golden/<id>.json.
#   2. No orphan corpus/*.pcap that no manifest entry references.
#   3. Payload leakage scan over corpus/*.pcap bytes (RFC1918 IPs, provider
#      keys, JWTs, PEM private keys, /Users|/home/<user> paths).
#
# Portable to bash 3.2 (no mapfile / associative arrays).
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
MANIFEST="$ROOT/testdata/pcaps/corpus.toml"
CORPUS="$ROOT/testdata/pcaps/corpus"
GOLDEN="$ROOT/testdata/pcaps/golden"

[ -f "$MANIFEST" ] || { echo "missing $MANIFEST"; exit 1; }

fail=0
note() { echo "check-pcap-corpus: $*"; }

ROWS_TMP=$(mktemp)
REF_TMP=$(mktemp)
trap 'rm -f "$ROWS_TMP" "$REF_TMP"' EXIT

# --- parse manifest (id|file|status); dependency-free, no tomllib (py3.8+) ---
python3 - "$MANIFEST" > "$ROWS_TMP" <<'PY'
import sys, re
rows, cur, capture = [], None, False
for line in open(sys.argv[1], encoding="utf-8"):
    s = line.strip()
    if s == "[[fixture]]":
        if cur is not None:
            rows.append(cur)
        cur, capture = {"id": "", "file": "", "status": "active"}, True
        continue
    if s.startswith("["):  # any nested sub-table ([fixture.expect] etc.)
        capture = False
        continue
    if cur is not None and capture:
        m = re.match(r'(\w+)\s*=\s*"(.*?)"', s)
        if m and m.group(1) in ("id", "file", "status"):
            cur[m.group(1)] = m.group(2)
if cur is not None:
    rows.append(cur)
for r in rows:
    print(f"{r['id']}|{r['file']}|{r['status']}")
PY

while IFS='|' read -r id file status; do
  [ -n "$id" ] || continue
  if [ "$id" = "ERR" ]; then echo "manifest parse error"; exit 1; fi
  if [ -z "$file" ]; then note "entry '$id' missing file"; fail=1; continue; fi
  echo "$file" >> "$REF_TMP"
  if [ "${status:-active}" = "active" ]; then
    [ -f "$CORPUS/$file" ] || { note "ACTIVE '$id' references missing corpus/$file"; fail=1; }
    [ -f "$GOLDEN/$id.json" ] || { note "ACTIVE '$id' missing golden/$id.json (run 'just corpus bless')"; fail=1; }
  fi
done < "$ROWS_TMP"

# --- no orphan pcaps --------------------------------------------------------
if [ -d "$CORPUS" ]; then
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    base=$(basename "$p")
    grep -qxF "$base" "$REF_TMP" || { note "orphan corpus/$base not in manifest"; fail=1; }
  done < <(find "$CORPUS" -name '*.pcap' 2>/dev/null)
fi

# --- payload leakage scan (skip git-LFS pointer files) ----------------------
scan_one() {
  f="$1"
  if head -c 64 "$f" 2>/dev/null | grep -q "version https://git-lfs"; then
    note "skip leakage scan (LFS pointer, not smudged): $(basename "$f")"; return 0
  fi
  # IP class MUST stay in sync with check-leakage.sh PRIV_IP_RE (RFC1918 + CGNAT
  # 100.64/10). CGNAT matters here — the host's tailscale range (100.x) is the
  # kind of internal addr a real capture could carry.
  hits=$(LC_ALL=C grep -aoE \
    'sk-(ant-)?[A-Za-z0-9_-]{12,}|eyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}|-----BEGIN [A-Z ]*PRIVATE KEY-----|\b(10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3}|192\.168\.[0-9]{1,3}\.[0-9]{1,3}|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3})\b' \
    "$f" 2>/dev/null | grep -avE '^X+$' | sort -u | head -20 || true)
  paths=$(LC_ALL=C grep -aoE '/(Users|home|root)/[A-Za-z0-9._-]+' "$f" 2>/dev/null | grep -avE '/(Users|home|root)/X+$' | sort -u | head -10 || true)
  if [ -n "$hits" ] || [ -n "$paths" ]; then
    note "LEAKAGE in $(basename "$f"):"
    [ -n "$hits" ] && echo "$hits" | sed 's/^/    /'
    [ -n "$paths" ] && echo "$paths" | sed 's/^/    /'
    return 1
  fi
  return 0
}

if [ -d "$CORPUS" ]; then
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    scan_one "$f" || fail=1
  done < <(find "$CORPUS" -name '*.pcap' 2>/dev/null)
fi

if [ "$fail" -ne 0 ]; then
  echo "check-pcap-corpus: ✗ FAILED"
  exit 1
fi
echo "check-pcap-corpus: ✓ manifest consistent, no leakage in corpus/*.pcap"
