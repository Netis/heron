#!/usr/bin/env bash
# Secret-value sanity linter.
#
# Companion to `check-secrets.sh` (which verifies the *existence* of
# referenced secrets). This one validates the *content* of each secret
# named on the command line, pulled from the environment by `${!NAME}`.
#
# Catches a specific real failure mode that burned hours of debug on
# 2026-05-26: when you set a secret via stdin like
#
#     printf '%s' "$VALUE" | gh secret set NAME -R repo --body -
#
# the shell quoting can swallow the pipe and `gh secret set NAME --body -`
# ends up persisting the literal **string `-`** (the stdin sentinel
# itself) as the secret value. The masking algorithm then replaces
# every `-` character in workflow logs with `***`, producing
# pathological symptoms like:
#
#   * `pre***flight` (the script word "pre-flight" masked)
#   * `curl: option ***/v1/models: is unknown` (curl chokes because
#     the BASE_URL expanded to literal `-`, prefixing the path with a
#     dash that curl interprets as a flag)
#   * `git remote set-url --push origin "https://x-access-token:${TOKEN}@…"`
#     evaluates to `…:x-access-token:-@…`, and pushes are rejected
#     with "Invalid username or token. Password authentication is not
#     supported for Git operations."
#
# The class of bug is invisible because the masked logs *look* like
# the secret was set correctly. This linter compares each secret's
# **length** and **shape** against sane bounds, refusing to forward
# obviously broken values into production code paths.
#
# Usage (intended from inside a workflow step):
#
#   - name: lint - secret values are sane
#     env:
#       AGENT_GH_TOKEN:   ${{ secrets.AGENT_GH_TOKEN }}
#       LITELLM_API_KEY:  ${{ secrets.LITELLM_API_KEY }}
#       LITELLM_BASE_URL: ${{ secrets.LITELLM_BASE_URL }}
#       LITELLM_NO_PROXY: ${{ secrets.LITELLM_NO_PROXY }}
#     run: bash scripts/lint/check-secret-values.sh \
#       AGENT_GH_TOKEN LITELLM_API_KEY LITELLM_BASE_URL LITELLM_NO_PROXY
#
# This script NEVER prints secret values — only their length and a
# coarse shape hint. Output is safe to commit to public CI logs.
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <ENV_VAR_NAME> [<ENV_VAR_NAME> ...]" >&2
  exit 2
fi

# Minimum length for a "real" secret. GitHub PATs are 40+ chars;
# LiteLLM keys are 30+; even a URL is well over 10. Anything under
# 8 is almost certainly a sentinel or placeholder.
MIN_LEN=8

bad=0
report_bad() {
  echo "::error::$1"
  bad=$((bad + 1))
}

for name in "$@"; do
  # Bash indirect expansion. The `-` default ensures `set -u` doesn't
  # explode on a missing var.
  val="${!name:-}"
  len=${#val}

  if [ "$len" -eq 0 ]; then
    report_bad "secret '$name' is empty in this workflow's env — provision it via 'gh secret set $name -R <repo> --body <value>' (don't use '--body -' from stdin without a heredoc; the pipe can silently fail)."
    continue
  fi

  if [ "$val" = "-" ]; then
    report_bad "secret '$name' has value '-' (single dash). Almost certainly a 'gh secret set $name --body -' stdin bug — the dash was persisted as the literal value. Reset with 'gh secret set $name -R <repo> --body \"<actual-value>\"' (NOT --body -)."
    continue
  fi

  if [ "$len" -lt "$MIN_LEN" ]; then
    report_bad "secret '$name' is suspiciously short ($len chars; expected ≥ $MIN_LEN). Probably a placeholder/sentinel left over from a setup script — verify and reset."
    continue
  fi

  # Looks plausible.
  echo "ok '$name' len=$len"
done

if [ "$bad" -gt 0 ]; then
  echo "::error::$bad secret(s) failed sanity checks; fix before merging."
  exit 1
fi

echo "check-secret-values: ✓ all $# secret(s) pass sanity checks"
