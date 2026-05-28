#!/usr/bin/env bash
# Orchestrate one PR review:
#   1) export PR_NUMBER / HEAD_SHA / BASE_REF for the prompt template
#   2) substitute them into prompt.md
#   3) run `claude -p` in print mode with the read-only tool allowlist
#   4) drop the model's stdout into /tmp/pr-review-${N}-out.md for
#      post_review.py to consume
#
# Exits non-zero only on infrastructure failure (LiteLLM unreachable,
# claude binary missing, etc). The post-review step inspects the
# output file content to decide review verdict.

set -euo pipefail

PR_NUMBER="${1:?usage: $0 <pr_number>}"
WORKDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

OUT="/tmp/pr-review-${PR_NUMBER}-out.md"
LOG="/tmp/pr-review-${PR_NUMBER}-agent.log"
PROMPT="/tmp/pr-review-${PR_NUMBER}-prompt.md"

# Resolve PR metadata for the prompt. The workflow has already
# checked out the head SHA — pull base_ref + head_sha back out of
# git so this script is also runnable locally (`bash run_review.sh
# 27` from a checkout) for development / debugging.
HEAD_SHA="$(git rev-parse HEAD)"
BASE_REF="$(gh pr view "$PR_NUMBER" --json baseRefName --jq .baseRefName)"

export PR_NUMBER HEAD_SHA BASE_REF
envsubst < "$WORKDIR/prompt.md" > "$PROMPT"

# Pre-flight: wait until LiteLLM is reachable AND our API key is
# accepted. The wait covers the common case where GLM-5 / LiteLLM
# is restarting when this workflow fires; if it's still down after
# 30 min (MAX_LITELLM_WAIT_SECONDS) the function returns 2 and we
# pass through with the same diagnostic shape as before. Shared
# helper in scripts/lib/litellm-wait.sh.
LITELLM_WAIT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/litellm-wait.sh"
# shellcheck source=../lib/litellm-wait.sh
source "$LITELLM_WAIT"
if ! wait_for_litellm; then
  echo "ERROR: agent unavailable (pre-flight)" > "$OUT"
  exit 2
fi

# Build the tool allowlist as a single comma-separated string (the
# format `claude --allowed-tools` accepts).
ALLOWED_TOOLS="$(grep -v '^#' "$WORKDIR/allowed_tools.txt" \
  | grep -v '^[[:space:]]*$' \
  | paste -sd, -)"

# Headless agent run. The 1800 s outer cap is a hard fence — if the
# model loops we'd rather post a "review timed out" than wedge the
# workflow.
#
# Feed the prompt over stdin instead of as a trailing positional arg —
# `--allowed-tools` is variadic (<tools...>), so a positional prompt
# right after it gets consumed as an extra tool name and claude then
# errors with "Input must be provided either through stdin or as a
# prompt argument when using --print".
#
# Retry on transient LiteLLM mid-stream failures: if claude exits
# non-zero AND the backend looks down right now (5xx / connect-
# refused / timeout — NOT 4xx), wait for LiteLLM and re-run. Review
# is idempotent (the OUT file just gets overwritten); restart from
# scratch is safe. Up to CLAUDE_RETRY_MAX retries (default 2).
CLAUDE_RETRY_MAX="${CLAUDE_RETRY_MAX:-2}"
attempt=0
claude_rc=0
while true; do
    set +e
    timeout 1800 claude \
      --print \
      --model "${ANTHROPIC_MODEL:-claude-3-5-sonnet-20241022}" \
      --max-turns 60 \
      --output-format text \
      --permission-mode acceptEdits \
      --allowed-tools "$ALLOWED_TOOLS" \
      < "$PROMPT" \
      > "$OUT" \
      2> "$LOG"
    claude_rc=$?
    set -e

    [ "$claude_rc" -eq 0 ] && break

    if ! litellm_appears_down; then
        # LiteLLM is up — failure is real (timeout, claude crash,
        # rate limit, etc). Don't retry.
        break
    fi

    if [ "$attempt" -ge "$CLAUDE_RETRY_MAX" ]; then
        echo "::error::vivi claude died $((attempt+1)) times with LiteLLM down; giving up" >&2
        break
    fi

    attempt=$((attempt + 1))
    echo "::warning::vivi claude exited $claude_rc, LiteLLM down; waiting + retrying (attempt $((attempt+1))/$((CLAUDE_RETRY_MAX+1)))" >&2
    wait_for_litellm || break
done

if [ "$claude_rc" -ne 0 ]; then
    echo "ERROR: agent exited with code $claude_rc" >> "$LOG"
    # Don't synthesize a PR-bound failure summary here — post_review.py
    # is gated on AGENT_EXIT and will skip the PR entirely on non-zero.
    # The OUT file is left as-is (possibly empty) for workflow-log
    # consumption.
    exit "$claude_rc"
fi

# Sanity: non-empty markdown with at least a `### Summary` heading.
if ! grep -q '^### Summary' "$OUT"; then
  echo "WARN: agent output missing ### Summary heading" >> "$LOG"
  printf '\n\n---\n_Agent output was missing required heading._\n' >> "$OUT"
fi

echo "review written to $OUT ($(wc -c < "$OUT") bytes)"
