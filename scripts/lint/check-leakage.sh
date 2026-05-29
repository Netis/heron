#!/usr/bin/env bash
# Sensitive-information leakage linter.
#
# A public repo must never carry internal-infra identity: private IPs,
# plaintext credentials, or private-key material. This linter is the
# deterministic gate behind CLAUDE.md's PR-hygiene rule — it fails CI
# on any tracked file that leaks one of the classes below.
#
# Detected classes (high-confidence, near-zero false positive):
#
#   1. Private / internal IPv4 addresses (RFC1918 + CGNAT) that are NOT
#      on the safe allow-list in scripts/lint/leakage-allowlist.txt.
#      Real infra hosts trip this; documentation ranges (RFC5737),
#      loopback, and the docker0 default do not.
#   2. Private-key PEM blocks (`-----BEGIN ... PRIVATE KEY-----`).
#
# Out of scope (left to the human/agent reviewer's semantic pass —
# regex can't separate these from legitimate prose):
#   * Plaintext passwords, internal hostnames, machine-specific paths.
#   The PR-review agent prompt carries a leakage dimension for those.
#
# Scope: tracked files only (git ls-files), minus vendored trees,
# build output, captured test fixtures (real wire data), historical
# design docs, the changelog, and lockfiles.
#
# Usage:
#   bash scripts/lint/check-leakage.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

ALLOWLIST="scripts/lint/leakage-allowlist.txt"

# Safe IP prefixes (string match), comments/blank stripped.
# (Plain `while read` rather than `mapfile` so this runs on bash 3.x too.)
SAFE_PREFIXES=()
while IFS= read -r _l; do SAFE_PREFIXES+=("$_l"); done < <(grep -vE '^\s*#|^\s*$' "$ALLOWLIST")

# Files to scan: tracked, minus the exclusions described above.
FILES=()
while IFS= read -r _f; do FILES+=("$_f"); done < <(
  git ls-files | grep -vE \
    'node_modules/|/target/|docs/superpowers/|tests/fixtures/|(^|/)CHANGELOG\.md$|\.lock$|(^|/)bun\.lock$|(^|/)package-lock\.json$|scripts/lint/leakage-allowlist\.txt$'
)

# RFC1918 + CGNAT (100.64/10) full-dotted-quad matcher.
PRIV_IP_RE='\b(10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3}|192\.168\.[0-9]{1,3}\.[0-9]{1,3}|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3})\b'

is_allowed_ip() {
  local ip="$1" pfx
  for pfx in "${SAFE_PREFIXES[@]}"; do
    case "$ip" in
      "$pfx"*) return 0 ;;
    esac
  done
  return 1
}

bad=0
report() {
  echo "::error::$1"
  bad=$((bad + 1))
}

for f in "${FILES[@]}"; do
  [ -f "$f" ] || continue

  # --- Class 1: non-allow-listed private IPs ---
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    lineno="${line%%:*}"
    rest="${line#*:}"
    # Extract each private IP on the line and test against the allowlist.
    while IFS= read -r ip; do
      [ -z "$ip" ] && continue
      if ! is_allowed_ip "$ip"; then
        report "$f:$lineno leaks private/internal IP '$ip' — replace with an RFC5737 doc range (192.0.2.x / 198.51.100.x / 203.0.113.x) or add the prefix to $ALLOWLIST if it is genuinely safe."
      fi
    done < <(grep -oE "$PRIV_IP_RE" <<<"$rest")
  done < <(grep -nE "$PRIV_IP_RE" "$f" 2>/dev/null || true)

  # --- Class 2: private-key PEM blocks ---
  if grep -nE -- '-----BEGIN ([A-Z0-9]+ )*PRIVATE KEY-----' "$f" >/dev/null 2>&1; then
    report "$f contains a PRIVATE KEY block — private keys must never be committed. Remove it and rotate the key."
  fi
done

if [ "$bad" -gt 0 ]; then
  echo "::error::$bad leakage issue(s) found; scrub before merging."
  exit 1
fi

echo "check-leakage: ✓ no private IPs or key material in ${#FILES[@]} tracked files"
