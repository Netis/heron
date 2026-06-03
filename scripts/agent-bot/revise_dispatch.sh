#!/usr/bin/env bash
# Called from the tail of pr-review.yml AFTER vivi posts her review.
# If vivi REQUEST_CHANGES'd a wiwi PR (label `auto-agent`), dispatch the
# `pr-revise` workflow so wiwi addresses the feedback automatically — unless
# the round cap is reached, in which case hand off to a human.
#
# Idempotent + safe to call for every PR: it bails out for human PRs, for
# non-CHANGES_REQUESTED verdicts, and for closed PRs.
#
# REQUIRES AGENT_GH_TOKEN (a PAT) as GH_TOKEN. A `gh workflow run` issued with
# the default GITHUB_TOKEN would be dropped by GitHub's anti-recursion rule
# (events from GITHUB_TOKEN don't start new workflow runs), and the dispatched
# revise run must itself be able to push + re-trigger ci/pr-review.
set -euo pipefail

PR="${PR_NUMBER:?PR_NUMBER required}"
# Number of times vivi may block a PR before we stop auto-revising and
# escalate. Counts CHANGES_REQUESTED reviews already on the PR (including the
# one just posted), so cap=3 gives wiwi two automated revision attempts.
MAX_REVISE_ROUNDS="${MAX_REVISE_ROUNDS:-3}"

meta=$(gh pr view "$PR" --json labels,reviews,state)

[ "$(echo "$meta" | jq -r '.state')" = "OPEN" ] || { echo "PR #$PR not open; skip"; exit 0; }
echo "$meta" | jq -r '.labels[].name' | grep -qx auto-agent || { echo "PR #$PR not auto-agent; skip"; exit 0; }

# vivi review states in chronological order. Identify vivi the same way
# auto_merge.sh does (login OR the vivi footer in the body).
vivi_states=$(echo "$meta" | jq -r '
  [ .reviews[]
    | select(.author.login=="vivi" or ((.body // "") | contains("vivi")))
    | .state ] | .[]')

latest=$(printf '%s\n' "$vivi_states" | grep -v '^$' | tail -1 || true)
if [ "$latest" != "CHANGES_REQUESTED" ]; then
  echo "vivi latest verdict=${latest:-none} (not CHANGES_REQUESTED); skip"
  exit 0
fi

rounds=$(printf '%s\n' "$vivi_states" | grep -c CHANGES_REQUESTED || true)

if [ "$rounds" -ge "$MAX_REVISE_ROUNDS" ]; then
  echo "revise rounds=$rounds ≥ cap=$MAX_REVISE_ROUNDS — escalating PR #$PR to a human"
  gh label create needs-human --color B60205 \
    --description "Auto-agent loop paused; needs a human" 2>/dev/null || true
  gh pr edit "$PR" --add-label needs-human >/dev/null 2>&1 || true
  gh pr comment "$PR" --body "🤖 vivi has requested changes ${rounds}× — at the auto-revise cap (\`${MAX_REVISE_ROUNDS}\`). Pausing the wiwi ↔ vivi loop and handing off to a human. To resume an automated pass, remove the \`needs-human\` label and re-run the \`pr-revise\` workflow for this PR."
  exit 0
fi

echo "vivi REQUEST_CHANGES on auto-agent PR #$PR (round $rounds/$MAX_REVISE_ROUNDS) → dispatching pr-revise"
gh workflow run pr-revise.yml -f pr_number="$PR"
