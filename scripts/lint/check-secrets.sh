#!/usr/bin/env bash
# Secret-reference linter.
#
# Catches the PR#45 failure class: a workflow references
# `${{ secrets.FOO }}` but FOO was never actually provisioned in the
# repo (or org), so the workflow explodes the first time a runner
# tries to use it.
#
# Behaviour
# ---------
# * Scans every `.github/workflows/*.yml` for `secrets.<NAME>`
#   references and collects the distinct set of names.
# * `GITHUB_TOKEN` is built-in to every workflow run and is excluded
#   from the missing check.
# * Discovers provisioned secrets at three layers:
#     1. Repo secrets: `gh secret list -R <repo>` (always queryable).
#     2. Org secrets visible to the repo: best-effort via
#        `gh api repos/<repo>/actions/organization-secrets` — needs a
#        PAT with `admin:org` to enumerate; in CI we typically *don't*
#        have that, so a 403 is downgraded to a warning rather than a
#        failure (we'll still catch repo-level misses, which is the
#        most common foot-gun).
#     3. Allow-list file `scripts/lint/secrets.allowlist` — one secret
#        name per line, used to declare secrets that legitimately live
#        somewhere unreachable from gh (e.g., environment-scoped
#        secrets). Comments after `#` are stripped.
# * Exits 1 when any referenced secret is missing from every layer.
#
# Usage
# -----
#   scripts/lint/check-secrets.sh                     # against $GITHUB_REPOSITORY or origin
#   scripts/lint/check-secrets.sh Netis/TokenScope    # explicit
set -euo pipefail

REPO="${1:-${GITHUB_REPOSITORY:-}}"
if [ -z "$REPO" ]; then
  REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)
fi
if [ -z "$REPO" ]; then
  echo "check-secrets: cannot determine repo (pass arg or set GITHUB_REPOSITORY)" >&2
  exit 2
fi

WORKFLOWS_DIR=".github/workflows"
ALLOWLIST="scripts/lint/secrets.allowlist"
if [ ! -d "$WORKFLOWS_DIR" ]; then
  echo "check-secrets: $WORKFLOWS_DIR not found (run from repo root)" >&2
  exit 2
fi

# 1. Collect referenced secret names.
referenced=$(grep -rhoE 'secrets\.[A-Z_][A-Z0-9_]*' "$WORKFLOWS_DIR" 2>/dev/null \
  | sed 's/^secrets\.//' | sort -u)

# Drop the built-in token.
referenced=$(echo "$referenced" | grep -vx 'GITHUB_TOKEN' || true)

if [ -z "$referenced" ]; then
  echo "check-secrets: no secret references found in $WORKFLOWS_DIR — nothing to check."
  exit 0
fi

# 2. Collect provisioned secrets, layer by layer.
repo_secrets=$(gh secret list -R "$REPO" --json name --jq '.[].name' 2>/dev/null \
  | sort -u || true)

# Best-effort: org secrets visible to the repo. The endpoint requires
# admin:org for full listing; without it we get nothing useful, so we
# only emit a warning, not an error.
org_secrets=""
org_status=$(gh api "repos/$REPO/actions/organization-secrets" --silent 2>&1 >/dev/null; echo $?)
if [ "$org_status" = "0" ]; then
  org_secrets=$(gh api "repos/$REPO/actions/organization-secrets" \
    --jq '.secrets[].name' 2>/dev/null | sort -u || true)
fi

allow_list=""
if [ -f "$ALLOWLIST" ]; then
  allow_list=$(sed 's/#.*//' "$ALLOWLIST" | awk 'NF' | sort -u)
fi

provisioned=$(printf '%s\n%s\n%s\n' "$repo_secrets" "$org_secrets" "$allow_list" \
  | awk 'NF' | sort -u)

# 3. Diff.
missing=$(comm -23 <(echo "$referenced") <(echo "$provisioned"))

if [ -n "$missing" ]; then
  echo "::error::check-secrets: $(echo "$missing" | wc -l | tr -d ' ') referenced secret(s) missing in $REPO"
  echo
  echo "Missing secrets (referenced by workflows but not provisioned):"
  echo "$missing" | sed 's/^/  - /'
  echo
  echo "Fix one of:"
  echo "  1. Provision the secret:    gh secret set <NAME> -R $REPO --body <value>"
  echo "  2. Org-level (if scoped):   gh secret set <NAME> --org Netis --visibility selected --repos $REPO"
  echo "  3. Allow-list (last resort): echo <NAME> >> $ALLOWLIST"
  echo
  echo "Why this matters: GitHub silently evaluates missing secrets to"
  echo "empty strings, which means actions/checkout / gh commands /"
  echo "auth flows that depend on them fail with cryptic errors only at"
  echo "the moment the workflow runs — often far from the PR that"
  echo "introduced the bad reference. This linter catches it at CI time."
  exit 1
fi

# Org-level visibility warning is informational only.
if [ "$org_status" != "0" ] && [ -z "$allow_list" ]; then
  echo "check-secrets: note — org-level secret enumeration unavailable" \
       "(token lacks admin:org)." >&2
  echo "check-secrets: relying on repo + allow-list layers only. If a" \
       "referenced secret lives at org scope, add it to $ALLOWLIST." >&2
fi

count=$(echo "$referenced" | wc -l | tr -d ' ')
echo "check-secrets: ✓ all $count referenced secrets provisioned in $REPO"
