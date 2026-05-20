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

# Pre-flight: verify LiteLLM is reachable AND our API key is
# accepted. The key check is what tells us the secret is wired
# correctly before we burn turns on the real agent run.
if ! curl -fsS --max-time 5 \
    -H "Authorization: Bearer ${ANTHROPIC_API_KEY:-}" \
    "${ANTHROPIC_BASE_URL:-}/v1/models" >/dev/null 2>&1; then
  echo "ERROR: LiteLLM unreachable or auth failed at ${ANTHROPIC_BASE_URL:-<unset>}" \
    | tee "$OUT" >&2
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
timeout 1800 claude \
  --print \
  --model "${ANTHROPIC_MODEL:-claude-3-5-sonnet-20241022}" \
  --max-turns 60 \
  --output-format text \
  --permission-mode acceptEdits \
  --allowed-tools "$ALLOWED_TOOLS" \
  "$(cat "$PROMPT")" \
  > "$OUT" \
  2> "$LOG" \
  || {
    rc=$?
    echo "ERROR: agent exited with code $rc" >> "$LOG"
    if [ ! -s "$OUT" ]; then
      printf '### Summary\nAgent run failed (exit %d). See workflow logs.\n' \
        "$rc" > "$OUT"
    fi
    exit "$rc"
  }

# Sanity: non-empty markdown with at least a `### Summary` heading.
if ! grep -q '^### Summary' "$OUT"; then
  echo "WARN: agent output missing ### Summary heading" >> "$LOG"
  printf '\n\n---\n_Agent output was missing required heading._\n' >> "$OUT"
fi

echo "review written to $OUT ($(wc -c < "$OUT") bytes)"
